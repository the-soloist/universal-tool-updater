use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::error::UpdaterError;

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
        filename: &str,
        downloaded: u64,
        sha256: &str,
    ) -> Result<DownloadedArtifact> {
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
        tracing::debug!(
            tool = %self.tool.id,
            artifact = self.index + 1,
            artifacts = self.artifacts,
            filename,
            url = %self.artifact.url,
            bytes = downloaded,
            sha256,
            path = %destination.display(),
            "artifact download completed and cached until installation succeeds"
        );
        Ok(DownloadedArtifact { path: destination })
    }
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
