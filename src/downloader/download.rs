use std::fs;
use std::io::{Read, Write};
use std::thread;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, ETAG, LAST_MODIFIED};
use sha2::{Digest, Sha256};

use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;
use crate::paths::{filename_from_url, safe_filename};
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

use super::completion::DownloadCompletion;
use super::http::{
    ATTEMPTS, RETRY_DELAY, byte_range, filename_from_disposition, response_header, send_with_retry,
    unsatisfied_total, validator_unchanged,
};
use super::partial::{
    Metadata as PartialMetadata, Verification, checkpoint as checkpoint_partial,
    clear as clear_partial, length as partial_length, load as load_partial_metadata,
    paths as partial_paths, prepare_resume,
};
use super::recovery::recover_previous_download;

const HASH_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;

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
        version: &str,
        artifact: &ResolvedArtifact,
        workspace: &ToolWorkspace,
        position: (usize, usize),
        progress: &TaskProgress,
    ) -> Result<DownloadedArtifact> {
        let (index, artifacts) = position;
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
        let (temporary, metadata_path) = partial_paths(partial_directory, &artifact.url);
        let mut metadata = load_partial_metadata(&metadata_path, &temporary, &artifact.url)?;
        let downloaded = partial_length(metadata.as_ref(), &temporary)?;
        if let Some((recovered, _recovered_length)) =
            recover_previous_download(tool, version, artifact, workspace, &temporary, downloaded)?
        {
            metadata = Some(recovered);
        }
        let resume = prepare_resume(&metadata_path, &temporary, metadata)?;
        let mut metadata = resume.metadata;
        let mut downloaded = resume.downloaded;
        let mut hasher = resume.hasher;

        let mut transfer_attempt = 1;
        loop {
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
                    let digest = checkpoint_partial(
                        &metadata_path,
                        partial,
                        requested_offset,
                        &hasher,
                        true,
                    )?;
                    progress.download(index + 1, artifacts, &partial.filename, remote_total);
                    progress.set_position(requested_offset);
                    return completion.finalize(
                        &temporary,
                        &partial.filename,
                        requested_offset,
                        &digest,
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
                hasher = Sha256::new();
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
                    hasher = Sha256::new();
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
                hasher = Sha256::new();
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
            let next_metadata = PartialMetadata {
                schema_version: super::partial::SCHEMA_VERSION,
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
                downloaded: None,
                sha256: None,
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
                hasher = Sha256::new();
            }
            let mut next_metadata = next_metadata;
            let mut last_checkpoint = downloaded;

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
                        hasher.update(&buffer[..read]);
                        progress.inc(read as u64);
                        if downloaded.saturating_sub(last_checkpoint) >= HASH_CHECKPOINT_BYTES {
                            output.sync_data().with_context(|| {
                                format!("cannot sync download file {}", temporary.display())
                            })?;
                            checkpoint_partial(
                                &metadata_path,
                                &mut next_metadata,
                                downloaded,
                                &hasher,
                                false,
                            )?;
                            last_checkpoint = downloaded;
                        }
                    }
                    Err(error) => break Some(format!("cannot read response body: {error}")),
                }
            };
            output
                .sync_all()
                .with_context(|| format!("cannot sync download file {}", temporary.display()))?;
            drop(output);
            checkpoint_partial(
                &metadata_path,
                &mut next_metadata,
                downloaded,
                &hasher,
                false,
            )?;

            let transfer_error = transfer_error.or_else(|| {
                response_end
                    .filter(|expected| downloaded != *expected)
                    .map(|expected| {
                        format!(
                            "download response ended at {downloaded}, expected {expected} bytes"
                        )
                    })
            });
            if let Some(message) = transfer_error {
                metadata = Some(next_metadata);
                if transfer_attempt < ATTEMPTS {
                    tracing::warn!(
                        tool = %tool.id,
                        attempt = transfer_attempt,
                        attempts = ATTEMPTS,
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
            if status == StatusCode::PARTIAL_CONTENT
                && total.is_some_and(|total| downloaded < total)
            {
                metadata = Some(next_metadata);
                transfer_attempt = 1;
                continue;
            }

            let digest = checkpoint_partial(
                &metadata_path,
                &mut next_metadata,
                downloaded,
                &hasher,
                true,
            )?;
            return completion.finalize(&temporary, &filename, downloaded, &digest);
        }
    }
}
