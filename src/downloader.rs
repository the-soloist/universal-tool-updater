use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, HeaderName, IF_RANGE, LAST_MODIFIED, RANGE,
};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tempfile::NamedTempFile;
use url::Url;

use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;
use crate::paths::safe_filename;
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

const DOWNLOAD_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);
const PARTIAL_SCHEMA_VERSION: u32 = 1;

pub struct Downloader {
    client: Client,
}

impl Downloader {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub(crate) fn download(
        &self,
        tool: &Tool,
        artifact: &ResolvedArtifact,
        workspace: &ToolWorkspace,
        index: usize,
        artifacts: usize,
        progress: &TaskProgress,
    ) -> Result<DownloadedArtifact> {
        let directory = workspace.downloads();
        let partial_directory = workspace.partials();
        fs::create_dir_all(partial_directory).with_context(|| {
            format!(
                "cannot create partial download directory {}",
                partial_directory.display()
            )
        })?;
        let (temporary, metadata_path) = partial_paths(partial_directory, &artifact.url);
        let mut metadata = load_partial_metadata(&metadata_path, &temporary, &artifact.url)?;
        let mut downloaded = partial_length(metadata.as_ref(), &temporary)?;

        if let Some(partial) = &metadata
            && partial.total == Some(downloaded)
            && downloaded > 0
        {
            progress.download(index + 1, artifacts, &partial.filename, partial.total);
            progress.set_position(downloaded);
            return finalize_download(
                tool,
                artifact,
                directory,
                &temporary,
                &metadata_path,
                &partial.filename,
                downloaded,
                index,
                artifacts,
            );
        }

        let mut transfer_attempt = 1;
        loop {
            let requested_offset = downloaded;
            let validator = metadata.as_ref().and_then(PartialMetadata::validator);
            let mut response =
                self.send_with_retry(tool, &artifact.url, requested_offset, validator)?;

            if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                let remote_total = unsatisfied_total(&response);
                if requested_offset > 0 && remote_total == Some(requested_offset) {
                    let partial = metadata.as_ref().context(
                        "partial download metadata disappeared before finalizing the download",
                    )?;
                    progress.download(index + 1, artifacts, &partial.filename, remote_total);
                    progress.set_position(requested_offset);
                    return finalize_download(
                        tool,
                        artifact,
                        directory,
                        &temporary,
                        &metadata_path,
                        &partial.filename,
                        requested_offset,
                        index,
                        artifacts,
                    );
                }

                tracing::warn!(
                    tool = %tool.id,
                    url = %artifact.url,
                    requested_offset,
                    remote_total = ?remote_total,
                    "server rejected the saved download range; restarting"
                );
                clear_partial(&metadata_path, &temporary)?;
                metadata = None;
                downloaded = 0;
                continue;
            }

            let status = response.status();
            if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
                return Err(UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!("server returned unexpected status {status}"),
                }
                .into());
            }
            let append = requested_offset > 0 && status == StatusCode::PARTIAL_CONTENT;
            let range = if status == StatusCode::PARTIAL_CONTENT {
                byte_range(&response)
            } else {
                None
            };
            if append {
                let valid_range = range.as_ref().is_some_and(|range| {
                    range.start == requested_offset
                        && response.content_length().is_none_or(|length| {
                            range
                                .end
                                .checked_sub(range.start)
                                .and_then(|length| length.checked_add(1))
                                == Some(length)
                        })
                        && metadata.as_ref().is_none_or(|partial| {
                            partial.total.is_none()
                                || range.total.is_none()
                                || partial.total == range.total
                        })
                });
                if !valid_range {
                    tracing::warn!(
                        tool = %tool.id,
                        url = %artifact.url,
                        requested_offset,
                        content_range = ?response.headers().get(CONTENT_RANGE),
                        "server returned an invalid download range; restarting"
                    );
                    clear_partial(&metadata_path, &temporary)?;
                    metadata = None;
                    downloaded = 0;
                    continue;
                }
            } else if requested_offset > 0 {
                tracing::debug!(
                    tool = %tool.id,
                    url = %artifact.url,
                    requested_offset,
                    status = %status,
                    "server did not resume the download; restarting from zero"
                );
                downloaded = 0;
            } else if status == StatusCode::PARTIAL_CONTENT
                && range.as_ref().is_none_or(|range| range.start != 0)
            {
                return Err(UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!(
                        "server returned an invalid Content-Range for {}",
                        artifact.url
                    ),
                }
                .into());
            }

            let filename = artifact
                .filename
                .as_deref()
                .and_then(safe_filename)
                .or_else(|| filename_from_disposition(&response))
                .or_else(|| {
                    metadata
                        .as_ref()
                        .and_then(|value| safe_filename(&value.filename))
                })
                .or_else(|| filename_from_url(&artifact.url))
                .unwrap_or_else(|| format!("artifact-{index}"));
            let total = if append {
                range
                    .as_ref()
                    .and_then(|range| range.total)
                    .or_else(|| metadata.as_ref().and_then(|value| value.total))
                    .or_else(|| {
                        response
                            .content_length()
                            .and_then(|remaining| downloaded.checked_add(remaining))
                    })
            } else {
                response.content_length()
            };
            let next_metadata = PartialMetadata {
                schema_version: PARTIAL_SCHEMA_VERSION,
                url: artifact.url.clone(),
                filename: filename.clone(),
                etag: response_header(&response, &ETAG)
                    .or_else(|| append.then(|| metadata.as_ref()?.etag.clone()).flatten()),
                last_modified: response_header(&response, &LAST_MODIFIED).or_else(|| {
                    append
                        .then(|| metadata.as_ref()?.last_modified.clone())
                        .flatten()
                }),
                total,
            };
            let mut output = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(&temporary)
                .with_context(|| format!("cannot open partial download {}", temporary.display()))?;
            if !append {
                downloaded = 0;
            }
            save_partial_metadata(&metadata_path, &next_metadata)?;
            metadata = Some(next_metadata);

            tracing::debug!(
                tool = %tool.id,
                artifact = index + 1,
                artifacts,
                filename,
                url = %artifact.url,
                resumed_from = downloaded,
                bytes = ?total,
                "artifact download started"
            );
            progress.download(index + 1, artifacts, &filename, total);
            progress.set_position(downloaded);

            let mut buffer = [0_u8; 64 * 1024];
            let transfer_error = loop {
                match response.read(&mut buffer) {
                    Ok(0) => break None,
                    Ok(read) => {
                        output.write_all(&buffer[..read]).with_context(|| {
                            format!("cannot write download file {}", temporary.display())
                        })?;
                        downloaded += read as u64;
                        progress.inc(read as u64);
                    }
                    Err(error) => break Some(format!("cannot read response body: {error}")),
                }
            };
            output
                .sync_all()
                .with_context(|| format!("cannot sync download file {}", temporary.display()))?;
            drop(output);

            let transfer_error = transfer_error.or_else(|| {
                total
                    .filter(|total| downloaded < *total)
                    .map(|total| format!("download ended at {downloaded} of {total} bytes"))
            });
            if let Some(message) = transfer_error {
                if transfer_attempt < DOWNLOAD_ATTEMPTS {
                    tracing::warn!(
                        tool = %tool.id,
                        attempt = transfer_attempt,
                        attempts = DOWNLOAD_ATTEMPTS,
                        downloaded,
                        error = message,
                        "download body failed; resuming"
                    );
                    thread::sleep(RETRY_DELAY * transfer_attempt as u32);
                    transfer_attempt += 1;
                    continue;
                }
                return Err(UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!(
                        "download from {} failed after {transfer_attempt} transfer attempt(s): {message}",
                        artifact.url
                    ),
                }
                .into());
            }
            if total.is_some_and(|total| downloaded > total) {
                clear_partial(&metadata_path, &temporary)?;
                return Err(UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!("download from {} exceeded the expected size", artifact.url),
                }
                .into());
            }

            return finalize_download(
                tool,
                artifact,
                directory,
                &temporary,
                &metadata_path,
                &filename,
                downloaded,
                index,
                artifacts,
            );
        }
    }

    fn send_with_retry(
        &self,
        tool: &Tool,
        url: &str,
        offset: u64,
        validator: Option<&str>,
    ) -> Result<Response> {
        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            let mut request = self.client.get(url);
            if offset > 0 {
                request = request.header(RANGE, format!("bytes={offset}-"));
                if let Some(validator) = validator {
                    request = request.header(IF_RANGE, validator);
                }
            }
            let response = request.send().and_then(|response| {
                if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                    Ok(response)
                } else {
                    response.error_for_status()
                }
            });
            match response {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let retry = attempt < DOWNLOAD_ATTEMPTS && is_retryable(&error);
                    if retry {
                        tracing::warn!(
                            tool = %tool.id,
                            attempt,
                            attempts = DOWNLOAD_ATTEMPTS,
                            error = %error,
                            "download request failed; retrying"
                        );
                        thread::sleep(RETRY_DELAY * attempt as u32);
                        continue;
                    }

                    let detail = format!("{:#}", anyhow::Error::new(error));
                    return Err(UpdaterError::Download {
                        tool: tool.id.clone(),
                        message: format!(
                            "request to {url} failed after {attempt} attempt(s): {detail}"
                        ),
                    }
                    .into());
                }
            }
        }

        unreachable!("download retry loop always returns")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PartialMetadata {
    schema_version: u32,
    url: String,
    filename: String,
    etag: Option<String>,
    last_modified: Option<String>,
    total: Option<u64>,
}

impl PartialMetadata {
    fn validator(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|etag| !etag.trim_start().starts_with("W/"))
            .or(self.last_modified.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

fn partial_paths(directory: &Path, url: &str) -> (PathBuf, PathBuf) {
    let mut digest = Sha1::new();
    digest.update(url.as_bytes());
    let key = format!("{:x}", digest.finalize());
    (
        directory.join(format!("{key}.part")),
        directory.join(format!("{key}.yaml")),
    )
}

fn load_partial_metadata(
    path: &Path,
    partial: &Path,
    url: &str,
) -> Result<Option<PartialMetadata>> {
    if !path.exists() || !partial.exists() {
        clear_partial(path, partial)?;
        return Ok(None);
    }
    let input = fs::read_to_string(path)
        .with_context(|| format!("cannot read partial download metadata {}", path.display()))?;
    let metadata = yaml_serde::from_str::<PartialMetadata>(&input);
    match metadata {
        Ok(metadata)
            if metadata.schema_version == PARTIAL_SCHEMA_VERSION
                && metadata.url == url
                && safe_filename(&metadata.filename).as_deref()
                    == Some(metadata.filename.as_str()) =>
        {
            Ok(Some(metadata))
        }
        Ok(_) | Err(_) => {
            tracing::warn!(path = %path.display(), "discarding invalid partial download metadata");
            clear_partial(path, partial)?;
            Ok(None)
        }
    }
}

fn save_partial_metadata(path: &Path, metadata: &PartialMetadata) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "cannot create partial metadata directory {}",
            parent.display()
        )
    })?;
    let encoded = yaml_serde::to_string(metadata).context("cannot encode partial metadata")?;
    let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "cannot create partial metadata file in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(encoded.as_bytes())
        .context("cannot write partial metadata")?;
    temporary
        .as_file()
        .sync_all()
        .context("cannot sync partial metadata")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace partial metadata {}", path.display()))?;
    Ok(())
}

fn partial_length(metadata: Option<&PartialMetadata>, partial: &Path) -> Result<u64> {
    if metadata.is_none() {
        return Ok(0);
    }
    fs::metadata(partial)
        .map(|value| value.len())
        .with_context(|| format!("cannot inspect partial download {}", partial.display()))
}

fn clear_partial(metadata: &Path, partial: &Path) -> Result<()> {
    for path in [metadata, partial] {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != ErrorKind::NotFound
        {
            return Err(error)
                .with_context(|| format!("cannot remove partial download {}", path.display()));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finalize_download(
    tool: &Tool,
    artifact: &ResolvedArtifact,
    directory: &Path,
    partial: &Path,
    metadata: &Path,
    filename: &str,
    downloaded: u64,
    index: usize,
    artifacts: usize,
) -> Result<DownloadedArtifact> {
    let destination = directory.join(filename);
    fs::rename(partial, &destination).with_context(|| {
        format!(
            "cannot finalize download {} -> {}",
            partial.display(),
            destination.display()
        )
    })?;
    if let Err(error) = fs::remove_file(metadata)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("cannot remove partial metadata {}", metadata.display()));
    }
    tracing::debug!(
        tool = %tool.id,
        artifact = index + 1,
        artifacts,
        filename,
        url = %artifact.url,
        bytes = downloaded,
        path = %destination.display(),
        "artifact download completed"
    );
    Ok(DownloadedArtifact { path: destination })
}

fn response_header(response: &Response, name: &HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn byte_range(response: &Response) -> Option<ByteRange> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    if end < start {
        return None;
    }
    let total = (total != "*").then(|| total.parse().ok()).flatten();
    Some(ByteRange { start, end, total })
}

fn unsatisfied_total(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes */")?
        .parse()
        .ok()
}

fn is_retryable(error: &reqwest::Error) -> bool {
    is_retryable_status(error.status())
}

fn is_retryable_status(status: Option<StatusCode>) -> bool {
    status.is_none_or(|status| {
        status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
    })
}

fn filename_from_disposition(response: &reqwest::blocking::Response) -> Option<String> {
    let value = response.headers().get(CONTENT_DISPOSITION)?.to_str().ok()?;
    value
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("filename="))
        .map(|name| name.trim_matches(['"', '\'']))
        .and_then(safe_filename)
}

fn filename_from_url(value: &str) -> Option<String> {
    Url::parse(value).ok().and_then(|url| {
        url.path_segments()
            .and_then(|mut parts| parts.next_back())
            .and_then(safe_filename)
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    use reqwest::StatusCode;
    use reqwest::blocking::Client;
    use tempfile::tempdir;

    use crate::config::model::{ArtifactConfig, HookConfig, ReleaseConfig};
    use crate::domain::{
        ExistingPolicy, InputMode, InstallSpec, OutputMode, ResolvedArtifact, Tool,
    };
    use crate::progress::ProgressManager;
    use crate::workspace::RunWorkspace;

    use super::{
        Downloader, PARTIAL_SCHEMA_VERSION, PartialMetadata, is_retryable_status, partial_paths,
        save_partial_metadata,
    };

    #[test]
    fn resumes_a_partial_download_from_a_previous_run() {
        let body = b"hello world";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            assert!(request.contains("range: bytes=6-"));
            assert!(request.contains("if-range: \"resume-v1\""));
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 6-10/11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.write_all(&body[6..]).unwrap();
        });

        let root = tempdir().unwrap();
        let tool = tool();
        let run = RunWorkspace::create(root.path()).unwrap();
        let workspace = run.prepare(&tool).unwrap();
        let (partial, metadata) = partial_paths(workspace.partials(), &url);
        fs::write(&partial, &body[..6]).unwrap();
        save_partial_metadata(
            &metadata,
            &PartialMetadata {
                schema_version: PARTIAL_SCHEMA_VERSION,
                url: url.clone(),
                filename: "artifact.bin".to_owned(),
                etag: Some("\"resume-v1\"".to_owned()),
                last_modified: None,
                total: Some(body.len() as u64),
            },
        )
        .unwrap();

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let progress = ProgressManager::new(false, 1);
        let task_progress = progress.task("test", "Resume");
        let downloaded = Downloader::new(client)
            .download(
                &tool,
                &ResolvedArtifact {
                    url,
                    filename: Some("artifact.bin".to_owned()),
                },
                &workspace,
                0,
                1,
                &task_progress,
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(downloaded.path).unwrap(), body);
        assert!(fs::read_dir(workspace.partials()).unwrap().next().is_none());
    }

    #[test]
    fn resumes_after_the_response_body_is_interrupted() {
        let body = b"hello world";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for request_number in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
                if request_number == 0 {
                    assert!(!request.contains("range:"));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    stream.write_all(&body[..6]).unwrap();
                } else {
                    assert!(request.contains("range: bytes=6-"));
                    assert!(request.contains("if-range: \"resume-v1\""));
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 6-10/11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    stream.write_all(&body[6..]).unwrap();
                }
            }
        });

        let root = tempdir().unwrap();
        let tool = tool();
        let run = RunWorkspace::create(root.path()).unwrap();
        let workspace = run.prepare(&tool).unwrap();
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let progress = ProgressManager::new(false, 1);
        let task_progress = progress.task("test", "Resume");
        let downloaded = Downloader::new(client)
            .download(
                &tool,
                &ResolvedArtifact {
                    url,
                    filename: Some("artifact.bin".to_owned()),
                },
                &workspace,
                0,
                1,
                &task_progress,
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(downloaded.path).unwrap(), body);
        assert!(fs::read_dir(workspace.partials()).unwrap().next().is_none());
    }

    #[test]
    fn retries_transport_and_transient_http_failures() {
        assert!(is_retryable_status(None));
        assert!(is_retryable_status(Some(StatusCode::REQUEST_TIMEOUT)));
        assert!(is_retryable_status(Some(StatusCode::TOO_MANY_REQUESTS)));
        assert!(is_retryable_status(Some(StatusCode::BAD_GATEWAY)));
    }

    #[test]
    fn does_not_retry_permanent_http_failures() {
        assert!(!is_retryable_status(Some(StatusCode::BAD_REQUEST)));
        assert!(!is_retryable_status(Some(StatusCode::NOT_FOUND)));
    }

    fn tool() -> Tool {
        Tool {
            id: "resume-test".to_owned(),
            name: "Resume Test".to_owned(),
            profile: "test".to_owned(),
            enabled: true,
            release: ReleaseConfig::Http {
                url: "https://example.com/artifact.bin".to_owned(),
                version_headers: Vec::new(),
            },
            artifacts: vec![ArtifactConfig::ReleaseUrl],
            install: InstallSpec {
                destination: PathBuf::from("resume-test"),
                input: InputMode::Copy,
                existing: ExistingPolicy::Replace,
                save: OutputMode::Directory,
                strip_single_root: true,
                create_destination: true,
                archive_name: "{name}-{version}.7z".to_owned(),
                archive_password: None,
                executable: Vec::new(),
                symlinks: Vec::new(),
            },
            hooks: HookConfig::default(),
        }
    }
}
