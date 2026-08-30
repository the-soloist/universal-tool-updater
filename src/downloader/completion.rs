use std::fmt::Write as _;
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;

use super::download::StreamedDigest;
use super::partial::clear as clear_partial;
use super::transfer::redact_url;

pub(crate) struct DownloadCompletion<'a> {
    pub(super) tool: &'a Tool,
    pub(super) artifact: &'a ResolvedArtifact,
    pub(super) directory: &'a Path,
    pub(super) index: usize,
    pub(super) artifacts: usize,
}

impl DownloadCompletion<'_> {
    pub(super) fn finalize(
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
        materialize_file(partial, &destination).with_context(|| {
            format!(
                "cannot materialize cached download {} -> {}",
                partial.display(),
                destination.display()
            )
        })?;
        let digest = streamed
            .filter(|digest| digest.synced_with(downloaded))
            .map(|digest| digest.prefix_digest());
        tracing::debug!(
            tool = %self.tool.id,
            artifact = self.index + 1,
            artifacts = self.artifacts,
            filename,
            url = %redact_url(&self.artifact.url),
            bytes = downloaded,
            sha256 = ?digest,
            path = %destination.display(),
            "artifact download completed and cached until installation succeeds"
        );
        Ok(DownloadedArtifact { path: destination })
    }
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

pub(super) fn materialize_file(source: &Path, destination: &Path) -> Result<()> {
    match fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(link_error) => replace_file(source, destination).with_context(|| {
            format!(
                "cannot copy cached download {} to {} after hard-link failed: {link_error}",
                source.display(),
                destination.display()
            )
        }),
    }
}

pub(super) fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().unwrap_or(Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "cannot create a temporary cache file in {}",
            parent.display()
        )
    })?;
    let mut input = fs::File::open(source)
        .with_context(|| format!("cannot open cached download {}", source.display()))?;
    std::io::copy(&mut input, temporary.as_file_mut()).with_context(|| {
        format!(
            "cannot copy cached download {} to {}",
            source.display(),
            temporary.path().display()
        )
    })?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("cannot sync cached download {}", temporary.path().display()))?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace cached download {}", destination.display()))?;
    Ok(())
}
