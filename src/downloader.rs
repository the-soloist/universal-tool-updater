use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::CONTENT_DISPOSITION;
use url::Url;

use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;
use crate::paths::safe_filename;
use crate::progress::TaskProgress;

const DOWNLOAD_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);

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
        directory: &Path,
        index: usize,
        artifacts: usize,
        progress: &TaskProgress,
    ) -> Result<DownloadedArtifact> {
        let mut response = self.send_with_retry(tool, &artifact.url)?;
        let filename = artifact
            .filename
            .as_deref()
            .and_then(safe_filename)
            .or_else(|| filename_from_disposition(&response))
            .or_else(|| filename_from_url(&artifact.url))
            .unwrap_or_else(|| format!("artifact-{index}"));
        let destination = directory.join(&filename);
        let temporary = destination.with_extension(format!(
            "{}.part",
            destination
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
        ));
        let total = response.content_length();
        tracing::debug!(
            tool = %tool.id,
            artifact = index + 1,
            artifacts,
            filename,
            url = %artifact.url,
            bytes = ?total,
            "artifact download started"
        );
        progress.download(index + 1, artifacts, &filename, total);
        let mut output = File::create(&temporary)
            .with_context(|| format!("cannot create download file {}", temporary.display()))?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut downloaded = 0_u64;
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|error| UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!("cannot read response body: {error}"),
                })?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .with_context(|| format!("cannot write download file {}", temporary.display()))?;
            downloaded += read as u64;
            progress.inc(read as u64);
        }
        output
            .sync_all()
            .with_context(|| format!("cannot sync download file {}", temporary.display()))?;
        fs::rename(&temporary, &destination).with_context(|| {
            format!(
                "cannot finalize download {} -> {}",
                temporary.display(),
                destination.display()
            )
        })?;
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

    fn send_with_retry(&self, tool: &Tool, url: &str) -> Result<Response> {
        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            match self
                .client
                .get(url)
                .send()
                .and_then(Response::error_for_status)
            {
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
    use reqwest::StatusCode;

    use super::is_retryable_status;

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
}
