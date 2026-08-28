use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::archive::{ArchiveService, archive_stem};
use crate::domain::{DownloadedArtifact, InputMode, Tool};
use crate::error::UpdaterError;

use super::filesystem::remove_path;

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
        remove_path(&output)?;
        fs::create_dir_all(&output)?;
        match tool.install.input {
            InputMode::Extract if self.archive.is_supported(&artifact.path) => {
                self.archive.extract(
                    &artifact.path,
                    &output,
                    tool.install.archive_password.as_deref(),
                )?;
                self.extract_nested_archive(&output, tool.install.archive_password.as_deref())?;
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

    fn extract_nested_archive(&self, directory: &Path, password: Option<&str>) -> Result<()> {
        let entries = fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
        if entries.len() != 1 {
            return Ok(());
        }
        let nested = entries[0].path();
        if !nested.is_file() || !self.archive.is_supported(&nested) {
            return Ok(());
        }
        self.archive.extract(&nested, directory, password)?;
        fs::remove_file(nested)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Write};

    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    use crate::archive::ArchiveService;
    use crate::domain::DownloadedArtifact;
    use crate::test_support::tool as test_tool;

    use super::ArtifactPreparer;

    #[test]
    fn extracts_a_single_nested_archive_and_removes_the_container() {
        let directory = tempdir().unwrap();
        let inner = zip_with_file("tool.txt", b"payload");
        let outer = directory.path().join("outer.zip");
        let file = fs::File::create(&outer).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("inner.zip", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&inner).unwrap();
        archive.finish().unwrap();

        let unpacked = directory.path().join("unpacked");
        fs::create_dir(&unpacked).unwrap();
        let tool = test_tool("nested", directory.path().join("destination"));
        let prepared = ArtifactPreparer::new(&ArchiveService, &unpacked)
            .prepare(&tool, &DownloadedArtifact { path: outer }, 0)
            .unwrap();

        assert_eq!(
            fs::read_to_string(prepared.join("tool.txt")).unwrap(),
            "payload"
        );
        assert!(!prepared.join("inner.zip").exists());
    }

    fn zip_with_file(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        archive.write_all(contents).unwrap();
        archive.finish().unwrap().into_inner()
    }
}
