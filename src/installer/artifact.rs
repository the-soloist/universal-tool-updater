use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::archive::{ArchiveService, archive_stem};
use crate::domain::{DownloadedArtifact, InputMode, Tool};
use crate::error::UpdaterError;

pub(super) struct ArtifactPreparer<'a> {
    archive: &'a ArchiveService,
    unpacked: &'a Path,
}

impl<'a> ArtifactPreparer<'a> {
    pub(super) fn new(archive: &'a ArchiveService, unpacked: &'a Path) -> Self {
        Self { archive, unpacked }
    }

    pub(super) fn prepare(
        &self,
        tool: &Tool,
        artifact: &DownloadedArtifact,
        index: usize,
    ) -> Result<PathBuf> {
        let stem = archive_stem(&artifact.path).unwrap_or_else(|| format!("artifact-{index}"));
        let output = self.unpacked.join(format!("{index:02}-{stem}"));
        if output.exists() {
            fs::remove_dir_all(&output)?;
        }
        fs::create_dir_all(&output)?;
        match tool.install.input {
            InputMode::Extract if self.archive.is_supported(&artifact.path) => {
                self.archive.extract(
                    &artifact.path,
                    &output,
                    tool.install.archive_password.as_deref(),
                )?;
                self.extract_nested_archive(&output)?;
            }
            InputMode::Extract | InputMode::Copy => {
                let name = artifact
                    .path
                    .file_name()
                    .ok_or_else(|| UpdaterError::Installation {
                        tool: tool.id.clone(),
                        message: format!("artifact {} has no filename", artifact.path.display()),
                    })?;
                fs::copy(&artifact.path, output.join(name))?;
            }
        }
        Ok(output)
    }

    fn extract_nested_archive(&self, directory: &Path) -> Result<()> {
        let entries = fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
        if entries.len() != 1 {
            return Ok(());
        }
        let nested = entries[0].path();
        if !nested.is_file() || !self.archive.is_supported(&nested) {
            return Ok(());
        }
        self.archive.extract(&nested, directory, None)?;
        fs::remove_file(nested)?;
        Ok(())
    }
}
