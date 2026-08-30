use std::fmt::Write as _;
use std::fs;
use std::io::{BufReader, ErrorKind, Read};
use std::path::Path;
use std::thread;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, ETAG, LAST_MODIFIED};
use sha2::{Digest, Sha256};
mod http;
mod partial;
pub(crate) mod transfer;

use crate::archive::Limits;
use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;
use crate::paths::{filename_from_url, safe_filename};
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

use http::{
    byte_range, filename_from_disposition, response_header, send_with_retry, unsatisfied_total,
    validator_unchanged,
};
use partial::{
    Metadata as PartialMetadata, SessionLock, clear as clear_partial, length as partial_length,
};
use partial::{
    empty_prefix_sha256, hash_prefix, load as load_partial_metadata, paths as partial_paths,
    save as save_partial_metadata,
};
use transfer::{ATTEMPTS, TransferFailure, backoff_delay, redact_url, stream_response};

/// Upper bound on zero-offset restarts caused by inconsistent servers.
const MAX_RESTARTS: usize = 3;

/// SHA-256 state kept in lockstep with the streamed bytes. `hashed_len`
/// counts the bytes fed to the hasher, so the digest equals the SHA-256 of
/// the first `hashed_len` file bytes only while that count matches the
/// current file length; otherwise callers fall back to a full re-read
/// (sessions resumed from a previous run, cross-restart rounds).
struct StreamedDigest {
    hasher: Sha256,
    hashed_len: u64,
}

impl Default for StreamedDigest {
    fn default() -> Self {
        Self {
            hasher: Sha256::new(),
            hashed_len: 0,
        }
    }
}

impl StreamedDigest {
    fn reset(&mut self) {
        self.hasher = Sha256::new();
        self.hashed_len = 0;
    }

    fn synced_with(&self, len: u64) -> bool {
        self.hashed_len == len
    }

    fn prefix_digest(&self) -> String {
        self.hasher
            .clone()
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

pub struct Downloader {
    client: Client,
    limits: Limits,
}

impl Downloader {
    pub fn new(client: Client, limits: Limits) -> Self {
        Self { client, limits }
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
        warn_insecure_transport(tool, &artifact.url);
        let directory = workspace.downloads();
        let completion = DownloadCompletion {
            tool,
            artifact,
            directory,
            index,
            artifacts,
        };
        let partial_directory = workspace.partials();
        fs::create_dir_all(partial_directory).with_context(|| {
            format!(
                "cannot create partial download directory {}",
                partial_directory.display()
            )
        })?;
        let _session_lock = SessionLock::acquire(partial_directory, &tool.id)?;
        let (temporary, metadata_path) = partial_paths(partial_directory, &artifact.url);
        let mut metadata = load_partial_metadata(&metadata_path, &temporary, &artifact.url)?;
        let mut downloaded = partial_length(metadata.as_ref(), &temporary)?;
        let mut streamed = StreamedDigest::default();

        let mut transfer_attempt = 1;
        let mut restarts = 0_usize;
        loop {
            let iteration_start = downloaded;
            let requested_offset = downloaded;
            let validator = metadata.as_ref().and_then(PartialMetadata::validator);
            let mut response = send_with_retry(
                &self.client,
                tool,
                &artifact.url,
                requested_offset,
                validator,
            )?;

            if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                let remote_total = unsatisfied_total(&response);
                if requested_offset > 0
                    && remote_total == Some(requested_offset)
                    && metadata
                        .as_ref()
                        .is_some_and(|partial| validator_unchanged(&response, partial.validator()))
                {
                    let partial = metadata.as_ref().context(
                        "partial download metadata disappeared before finalizing the download",
                    )?;
                    progress.download(index + 1, artifacts, &partial.filename, remote_total);
                    progress.set_position(requested_offset);
                    return completion.finalize(
                        &temporary,
                        &metadata_path,
                        &partial.filename,
                        requested_offset,
                        // The bytes came from a previous session; no in-run
                        // digest covers them.
                        None,
                    );
                }

                count_restart(tool, &artifact.url, &mut restarts)?;
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
                streamed.reset();
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
            let range = if status == StatusCode::PARTIAL_CONTENT {
                byte_range(&response)
            } else {
                None
            };
            if status == StatusCode::PARTIAL_CONTENT {
                let valid_range = range.as_ref().is_some_and(|range| {
                    range.start == requested_offset
                        && response
                            .content_length()
                            .is_none_or(|length| range.response_length() == Some(length))
                        && range.total.is_none_or(|total| range.end < total)
                        && metadata.as_ref().is_none_or(|partial| {
                            requested_offset == 0
                                || partial.total.is_none()
                                || range.total.is_none()
                                || partial.total == range.total
                        })
                        && range.end_offset().is_some()
                });
                if !valid_range {
                    if requested_offset == 0 {
                        return Err(UpdaterError::Download {
                            tool: tool.id.clone(),
                            message: format!(
                                "server returned an invalid Content-Range for {}",
                                artifact.url
                            ),
                        }
                        .into());
                    }
                    count_restart(tool, &artifact.url, &mut restarts)?;
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
                    streamed.reset();
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
                streamed.reset();
            }
            let append = requested_offset > 0 && status == StatusCode::PARTIAL_CONTENT;

            let filename = artifact
                .filename
                .as_deref()
                .and_then(safe_filename)
                .or_else(|| filename_from_disposition(&response))
                .or_else(|| filename_from_url(response.url().as_str()))
                .or_else(|| {
                    metadata
                        .as_ref()
                        .and_then(|value| safe_filename(&value.filename))
                })
                .or_else(|| filename_from_url(&artifact.url))
                .unwrap_or_else(|| format!("artifact-{index}"));
            let response_end = range
                .as_ref()
                .and_then(|range| range.end_offset())
                .or_else(|| {
                    response
                        .content_length()
                        .and_then(|length| downloaded.checked_add(length))
                });
            let total = if status == StatusCode::PARTIAL_CONTENT {
                range
                    .as_ref()
                    .and_then(|range| range.total)
                    .or_else(|| metadata.as_ref().and_then(|value| value.total))
                    .or(response_end)
            } else {
                response.content_length()
            };
            if let Some(total) = total.filter(|total| *total > self.limits.max_total_bytes) {
                return Err(UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!(
                        "download from {} reports {total} bytes, exceeding the extraction limit of {} bytes",
                        artifact.url, self.limits.max_total_bytes
                    ),
                }
                .into());
            }
            // The prefix digest covers the bytes already on disk: a resumed
            // transfer carries the digest verified at load time forward,
            // while a fresh transfer starts from the empty digest. Bytes
            // streamed this round are hashed as they arrive and folded into
            // the metadata once per round, keeping the hashing linear.
            let (prefix_sha256, prefix_len) = if append {
                let partial = metadata
                    .as_ref()
                    .expect("a resumed transfer keeps its verified metadata");
                (partial.prefix_sha256.clone(), partial.prefix_len)
            } else {
                (empty_prefix_sha256(), 0)
            };
            let next_metadata = PartialMetadata {
                schema_version: partial::SCHEMA_VERSION,
                filename: filename.clone(),
                etag: response_header(&response, &ETAG)
                    .or_else(|| append.then(|| metadata.as_ref()?.etag.clone()).flatten()),
                last_modified: response_header(&response, &LAST_MODIFIED).or_else(|| {
                    append
                        .then(|| metadata.as_ref()?.last_modified.clone())
                        .flatten()
                }),
                total,
                prefix_sha256,
                prefix_len,
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
                streamed.reset();
            }
            save_partial_metadata(&metadata_path, &next_metadata)?;
            metadata = Some(next_metadata);

            tracing::debug!(
                tool = %tool.id,
                artifact = index + 1,
                artifacts,
                filename,
                url = %redact_url(&artifact.url),
                resumed_from = downloaded,
                bytes = ?total,
                "artifact download started"
            );
            progress.download(index + 1, artifacts, &filename, total);
            progress.set_position(downloaded);

            let on_chunk = |chunk: &[u8]| {
                streamed.hasher.update(chunk);
                streamed.hashed_len += chunk.len() as u64;
                progress.inc(chunk.len() as u64);
            };
            let transferred = stream_response(
                &mut response,
                &mut output,
                &temporary,
                downloaded,
                response_end,
                self.limits.max_total_bytes,
                on_chunk,
            );
            drop(output);

            let transfer_error = match transferred {
                Ok(written) => {
                    downloaded = written;
                    None
                }
                Err(TransferFailure::Retryable { written, message }) => {
                    downloaded = written;
                    Some(message)
                }
                Err(TransferFailure::Fatal(error)) => return Err(error),
                Err(TransferFailure::LimitExceeded { written }) => {
                    clear_partial(&metadata_path, &temporary)?;
                    return Err(UpdaterError::Download {
                        tool: tool.id.clone(),
                        message: format!(
                            "download from {} wrote {written} bytes, exceeding the extraction limit of {} bytes",
                            artifact.url, self.limits.max_total_bytes
                        ),
                    }
                    .into());
                }
            };
            if let Some(message) = transfer_error {
                if transfer_attempt < ATTEMPTS {
                    tracing::warn!(
                        tool = %tool.id,
                        attempt = transfer_attempt,
                        attempts = ATTEMPTS,
                        downloaded,
                        error = message,
                        "download body failed; resuming"
                    );
                    thread::sleep(backoff_delay(transfer_attempt));
                    refresh_partial_prefix(
                        &temporary,
                        &metadata_path,
                        metadata.as_mut(),
                        Some(&streamed),
                    )?;
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
            if status == StatusCode::PARTIAL_CONTENT
                && total.is_some_and(|total| downloaded < total)
            {
                if downloaded <= iteration_start {
                    return Err(UpdaterError::Download {
                        tool: tool.id.clone(),
                        message: format!(
                            "download from {} made no progress in the last transfer round",
                            artifact.url
                        ),
                    }
                    .into());
                }
                refresh_partial_prefix(
                    &temporary,
                    &metadata_path,
                    metadata.as_mut(),
                    Some(&streamed),
                )?;
                continue;
            }

            return completion.finalize(
                &temporary,
                &metadata_path,
                &filename,
                downloaded,
                Some(&streamed),
            );
        }
    }
}

fn warn_insecure_transport(tool: &Tool, url: &str) {
    if let Ok(parsed) = url::Url::parse(url)
        && parsed.scheme() == "http"
    {
        tracing::warn!(
            tool = %tool.id,
            url = %redact_url(url),
            "downloading over plain HTTP; the connection is not encrypted and the download can be tampered with (allow_insecure_transports is enabled)"
        );
    }
}

fn count_restart(tool: &Tool, url: &str, restarts: &mut usize) -> Result<()> {
    if *restarts >= MAX_RESTARTS {
        return Err(UpdaterError::Download {
            tool: tool.id.clone(),
            message: format!(
                "download from {url} restarted {} time(s) without completing; restart limit is {MAX_RESTARTS}",
                *restarts
            ),
        }
        .into());
    }
    *restarts += 1;
    Ok(())
}

/// Re-hashes the grown partial and re-saves the metadata so the next session
/// can verify the resume point. Runs once per transfer round, keeping the
/// hashing cost linear per round instead of per metadata write. When the
/// streaming digest covers the whole file, its in-memory state replaces the
/// re-read.
fn refresh_partial_prefix(
    temporary: &Path,
    metadata_path: &Path,
    metadata: Option<&mut PartialMetadata>,
    streamed: Option<&StreamedDigest>,
) -> Result<()> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let len = fs::metadata(temporary)
        .with_context(|| format!("cannot inspect partial download {}", temporary.display()))?
        .len();
    metadata.prefix_sha256 = match streamed.filter(|digest| digest.synced_with(len)) {
        Some(digest) => digest.prefix_digest(),
        None => hash_prefix(temporary, len)?,
    };
    metadata.prefix_len = len;
    save_partial_metadata(metadata_path, metadata)
}

fn verify_sha256(path: &Path, expected: &str) -> std::result::Result<(), String> {
    let file =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut actual = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut actual, "{byte:02x}").expect("writing to a String cannot fail");
    }
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "sha256 checksum mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

struct DownloadCompletion<'a> {
    tool: &'a Tool,
    artifact: &'a ResolvedArtifact,
    directory: &'a Path,
    index: usize,
    artifacts: usize,
}

impl DownloadCompletion<'_> {
    fn finalize(
        &self,
        partial: &Path,
        metadata: &Path,
        filename: &str,
        downloaded: u64,
        streamed: Option<&StreamedDigest>,
    ) -> Result<DownloadedArtifact> {
        if let Some(expected) = &self.artifact.expected_sha256 {
            // The streaming digest is authoritative only when it covers the
            // exact file length; anything else re-reads the file.
            let memory = streamed.filter(|digest| digest.synced_with(downloaded));
            let verification = match memory.map(|digest| digest.prefix_digest()) {
                Some(actual) if actual.eq_ignore_ascii_case(expected) => Ok(()),
                Some(actual) => Err(format!(
                    "sha256 checksum mismatch: expected {expected}, got {actual}"
                )),
                None => verify_sha256(partial, expected),
            };
            if let Err(message) = verification {
                clear_partial(metadata, partial)?;
                return Err(UpdaterError::Download {
                    tool: self.tool.id.clone(),
                    message: format!("{message} for {}", self.artifact.url),
                }
                .into());
            }
        }
        let destination = self.directory.join(filename);
        if destination.exists() {
            return Err(UpdaterError::Download {
                tool: self.tool.id.clone(),
                message: format!("multiple artifacts resolved to the same filename {filename:?}"),
            }
            .into());
        }
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
            tool = %self.tool.id,
            artifact = self.index + 1,
            artifacts = self.artifacts,
            filename,
            url = %redact_url(&self.artifact.url),
            bytes = downloaded,
            path = %destination.display(),
            "artifact download completed"
        );
        Ok(DownloadedArtifact { path: destination })
    }
}

#[cfg(test)]
mod tests;
