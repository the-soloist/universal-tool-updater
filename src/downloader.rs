use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::thread;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, ETAG, LAST_MODIFIED};
mod http;
mod partial;

use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;
use crate::paths::{filename_from_url, safe_filename};
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

use http::{
    ATTEMPTS, RETRY_DELAY, byte_range, filename_from_disposition, response_header, send_with_retry,
    unsatisfied_total, validator_unchanged,
};
use partial::{Metadata as PartialMetadata, clear as clear_partial, length as partial_length};
use partial::{
    load as load_partial_metadata, paths as partial_paths, save as save_partial_metadata,
};

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
        let mut downloaded = partial_length(metadata.as_ref(), &temporary)?;

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
            }
            let append = requested_offset > 0 && status == StatusCode::PARTIAL_CONTENT;

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
                schema_version: partial::SCHEMA_VERSION,
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
                response_end
                    .filter(|expected| downloaded != *expected)
                    .map(|expected| {
                        format!(
                            "download response ended at {downloaded}, expected {expected} bytes"
                        )
                    })
            });
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
                transfer_attempt = 1;
                continue;
            }

            return completion.finalize(&temporary, &metadata_path, &filename, downloaded);
        }
    }
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
    ) -> Result<DownloadedArtifact> {
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
            url = %self.artifact.url,
            bytes = downloaded,
            path = %destination.display(),
            "artifact download completed"
        );
        Ok(DownloadedArtifact { path: destination })
    }
}

#[cfg(test)]
mod tests;
