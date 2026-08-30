use std::fs;
use std::thread;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, ETAG, LAST_MODIFIED};
use sha2::{Digest as _, Sha256};

use crate::archive::Limits;
use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;
use crate::paths::{filename_from_url, safe_filename};
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

use super::completion::DownloadCompletion;
use super::http::{
    byte_range, filename_from_disposition, response_header, send_with_retry, unsatisfied_total,
    validator_unchanged,
};
use super::partial::{
    Metadata as PartialMetadata, ResumeState, SessionLock, Verification,
    checkpoint as checkpoint_partial, clear as clear_partial, empty_prefix_sha256,
    length as partial_length, load as load_partial_metadata, paths as partial_paths,
    prepare_resume, save as save_partial_metadata,
};
use super::recovery::recover_previous_download;
use super::transfer::{ATTEMPTS, TransferFailure, backoff_delay, redact_url, stream_response};

/// Interval between persisted SHA-256 checkpoints during a transfer: bounds
/// the re-download volume after a crash to this many bytes.
const HASH_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;

/// Upper bound on zero-offset restarts caused by inconsistent servers.
const MAX_RESTARTS: usize = 3;

/// SHA-256 state kept in lockstep with the streamed bytes. `hashed_len`
/// counts the bytes fed to the hasher, so the digest equals the SHA-256 of
/// the first `hashed_len` file bytes only while that count matches the
/// current file length; otherwise callers fall back to a full re-read.
pub(super) struct StreamedDigest {
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

    pub(super) fn synced_with(&self, len: u64) -> bool {
        self.hashed_len == len
    }

    pub(super) fn prefix_digest(&self) -> String {
        let digest = self.hasher.clone().finalize();
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
        version: &str,
        artifact: &ResolvedArtifact,
        workspace: &ToolWorkspace,
        position: (usize, usize),
        progress: &TaskProgress,
    ) -> Result<DownloadedArtifact> {
        let (index, artifacts) = position;
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
        let downloaded = partial_length(metadata.as_ref(), &temporary)?;
        if let Some((recovered, _recovered_length)) =
            recover_previous_download(tool, version, artifact, workspace, &temporary, downloaded)?
        {
            metadata = Some(recovered);
        }
        let ResumeState {
            metadata: resumed_metadata,
            downloaded: resumed_length,
            hasher,
        } = prepare_resume(&metadata_path, &temporary, metadata)?;
        let mut metadata = resumed_metadata;
        let mut downloaded = resumed_length;
        let mut streamed = StreamedDigest {
            hasher,
            hashed_len: resumed_length,
        };

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
                    let partial = metadata.as_mut().context(
                        "partial download metadata disappeared before finalizing the download",
                    )?;
                    partial.total = remote_total;
                    checkpoint_partial(
                        &metadata_path,
                        partial,
                        requested_offset,
                        &streamed.hasher,
                        true,
                    )?;
                    progress.download(index + 1, artifacts, &partial.filename, remote_total);
                    progress.set_position(requested_offset);
                    return completion.finalize(
                        &temporary,
                        &metadata_path,
                        &partial.filename,
                        requested_offset,
                        Some(&streamed),
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
                    // 错误的范围响应若直接追加会静默破坏文件，因此丢弃断点状态并从零开始请求。
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
            // the metadata at periodic checkpoints, keeping the hashing
            // linear.
            let (prefix_sha256, prefix_len) = if append {
                let partial = metadata
                    .as_ref()
                    .expect("a resumed transfer keeps its verified metadata");
                (partial.prefix_sha256.clone(), partial.prefix_len)
            } else {
                (empty_prefix_sha256(), 0)
            };
            let next_metadata = PartialMetadata {
                schema_version: super::partial::SCHEMA_VERSION,
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
                complete: false,
                verified: Verification::None,
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
            let mut last_checkpoint = downloaded;

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

            let on_chunk = |chunk: &[u8], output: &mut fs::File| -> Result<()> {
                streamed.hasher.update(chunk);
                streamed.hashed_len += chunk.len() as u64;
                progress.inc(chunk.len() as u64);
                if streamed.hashed_len.saturating_sub(last_checkpoint) >= HASH_CHECKPOINT_BYTES {
                    // Sync the data file before the metadata checkpoint so a
                    // crash never leaves the checkpoint ahead of the bytes.
                    output.sync_data().with_context(|| {
                        format!("cannot sync download file {}", temporary.display())
                    })?;
                    let current = metadata
                        .as_mut()
                        .expect("streaming keeps its metadata present");
                    checkpoint_partial(
                        &metadata_path,
                        current,
                        streamed.hashed_len,
                        &streamed.hasher,
                        false,
                    )?;
                    last_checkpoint = streamed.hashed_len;
                }
                Ok(())
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
            let round_synced = match &transferred {
                Err(TransferFailure::LimitExceeded { .. }) => None,
                _ => Some(output.sync_all()),
            };
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
            if let Some(synced) = round_synced {
                synced.with_context(|| {
                    format!("cannot sync download file {}", temporary.display())
                })?;
            }
            // Persist the exact bytes of this round so an interrupted run
            // resumes from here instead of the last periodic checkpoint.
            if let Some(current) = metadata.as_mut() {
                checkpoint_partial(&metadata_path, current, downloaded, &streamed.hasher, false)?;
            }
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
                continue;
            }

            checkpoint_partial(
                &metadata_path,
                metadata
                    .as_mut()
                    .expect("a completed round keeps its metadata"),
                downloaded,
                &streamed.hasher,
                true,
            )?;
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
