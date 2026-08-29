use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::{Builder, TempDir};

use crate::domain::Tool;

pub(crate) struct RunWorkspace {
    directory: TempDir,
    staging: TempDir,
    downloads_root: PathBuf,
    partials: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ToolWorkspace {
    downloads: PathBuf,
    unpacked: PathBuf,
    staging: PathBuf,
    downloads_root: PathBuf,
    run_directory: PathBuf,
    tool_id: String,
    partials: PathBuf,
}

impl RunWorkspace {
    pub(crate) fn create(downloads_root: &Path, staging_root: &Path) -> Result<Self> {
        fs::create_dir_all(downloads_root).with_context(|| {
            format!(
                "cannot create update directory {}",
                downloads_root.display()
            )
        })?;
        fs::create_dir_all(staging_root).with_context(|| {
            format!("cannot create staging directory {}", staging_root.display())
        })?;
        let directory = Builder::new()
            .prefix("run-")
            .tempdir_in(downloads_root)
            .with_context(|| {
                format!(
                    "cannot create run directory in {}",
                    downloads_root.display()
                )
            })?;
        let staging = Builder::new()
            .prefix("run-")
            .tempdir_in(staging_root)
            .with_context(|| {
                format!(
                    "cannot create staging run directory in {}",
                    staging_root.display()
                )
            })?;
        Ok(Self {
            directory,
            staging,
            downloads_root: downloads_root.to_path_buf(),
            partials: downloads_root.join(".partial"),
        })
    }

    pub(crate) fn prepare(&self, tool: &Tool) -> Result<ToolWorkspace> {
        let root = self.directory.path().join(&tool.id);
        let downloads = root.join("downloads");
        let unpacked = root.join("unpacked");
        let staging = self.staging.path().join(&tool.id);
        let partials = self.partials.join(&tool.id);
        fs::create_dir_all(&downloads).with_context(|| {
            format!(
                "cannot create download directory for {} at {}",
                tool.id,
                downloads.display()
            )
        })?;
        fs::create_dir_all(&unpacked).with_context(|| {
            format!(
                "cannot create unpack directory for {} at {}",
                tool.id,
                unpacked.display()
            )
        })?;
        fs::create_dir(&staging).with_context(|| {
            format!(
                "cannot create transaction staging directory for {} at {}",
                tool.id,
                staging.display()
            )
        })?;
        fs::create_dir_all(&partials).with_context(|| {
            format!(
                "cannot create partial download directory for {} at {}",
                tool.id,
                partials.display()
            )
        })?;
        Ok(ToolWorkspace {
            downloads,
            unpacked,
            staging,
            downloads_root: self.downloads_root.clone(),
            run_directory: self.directory.path().to_path_buf(),
            tool_id: tool.id.clone(),
            partials,
        })
    }
}

impl ToolWorkspace {
    pub(crate) fn downloads(&self) -> &Path {
        &self.downloads
    }

    pub(crate) fn unpacked(&self) -> &Path {
        &self.unpacked
    }

    pub(crate) fn staging(&self) -> &Path {
        &self.staging
    }

    pub(crate) fn partials(&self) -> &Path {
        &self.partials
    }

    pub(crate) fn recoverable_download(&self, filename: &str) -> Result<Option<PathBuf>> {
        let mut largest = None::<(u64, PathBuf)>;
        for entry in fs::read_dir(&self.downloads_root).with_context(|| {
            format!(
                "cannot inspect previous runs in {}",
                self.downloads_root.display()
            )
        })? {
            let entry = entry.with_context(|| {
                format!(
                    "cannot inspect an entry in {}",
                    self.downloads_root.display()
                )
            })?;
            let path = entry.path();
            if path == self.run_directory
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("run-"))
                || !entry
                    .file_type()
                    .with_context(|| format!("cannot inspect {}", path.display()))?
                    .is_dir()
            {
                continue;
            }

            let candidate = path.join(&self.tool_id).join("downloads").join(filename);
            let metadata = match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("cannot inspect previous download {}", candidate.display())
                    });
                }
            };
            if largest
                .as_ref()
                .is_none_or(|(length, _)| metadata.len() > *length)
            {
                largest = Some((metadata.len(), candidate));
            }
        }
        Ok(largest.map(|(_, path)| path))
    }

    pub(crate) fn clear_partials(&self) -> Result<()> {
        match fs::symlink_metadata(&self.partials) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                fs::remove_dir_all(&self.partials).with_context(|| {
                    format!(
                        "cannot clear partial download directory {}",
                        self.partials.display()
                    )
                })?;
            }
            Ok(_) => anyhow::bail!(
                "partial download path {} is not a directory",
                self.partials.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot inspect partial download directory {}",
                        self.partials.display()
                    )
                });
            }
        }
        Ok(())
    }

    pub(crate) fn clear_downloads(&self) -> Result<()> {
        match fs::symlink_metadata(&self.downloads) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                fs::remove_dir_all(&self.downloads).with_context(|| {
                    format!(
                        "cannot clear completed download directory {}",
                        self.downloads.display()
                    )
                })?;
            }
            Ok(_) => anyhow::bail!(
                "download path {} is not a directory",
                self.downloads.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot inspect completed download directory {}",
                        self.downloads.display()
                    )
                });
            }
        }
        Ok(())
    }
}

impl Drop for ToolWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.partials);
        if let Some(root) = self.partials.parent() {
            let _ = fs::remove_dir(root);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::test_support::tool;

    use super::RunWorkspace;

    #[test]
    fn tool_directories_are_isolated_while_partial_downloads_persist() {
        let root = tempdir().unwrap();
        let staging = root.path().join("staging");
        let run_path;
        let staging_run_path;
        let partial_path;
        {
            let run = RunWorkspace::create(root.path(), &staging).unwrap();
            let first = run.prepare(&tool("first", "first")).unwrap();
            let second = run.prepare(&tool("second", "second")).unwrap();
            run_path = first
                .downloads()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            staging_run_path = first.staging().parent().unwrap().to_path_buf();

            fs::write(first.downloads().join("artifact.zip"), "first").unwrap();
            fs::write(second.downloads().join("artifact.zip"), "second").unwrap();
            partial_path = first.partials().join("artifact.part");
            fs::write(&partial_path, "partial").unwrap();

            assert_eq!(
                fs::read_to_string(first.downloads().join("artifact.zip")).unwrap(),
                "first"
            );
            assert_eq!(
                fs::read_to_string(second.downloads().join("artifact.zip")).unwrap(),
                "second"
            );
            assert_ne!(first.downloads().parent(), second.downloads().parent());
            assert_ne!(first.staging(), second.staging());
            assert!(first.staging().starts_with(&staging));
        }
        assert!(!run_path.exists());
        assert!(!staging_run_path.exists());
        assert_eq!(fs::read_to_string(partial_path).unwrap(), "partial");
    }

    #[test]
    fn clears_only_the_completed_downloads_for_one_tool() {
        let root = tempdir().unwrap();
        let staging = root.path().join("staging");
        let run = RunWorkspace::create(root.path(), &staging).unwrap();
        let first = run.prepare(&tool("first", "first")).unwrap();
        let second = run.prepare(&tool("second", "second")).unwrap();
        fs::write(first.downloads().join("artifact.zip"), "first").unwrap();
        fs::write(second.downloads().join("artifact.zip"), "second").unwrap();

        first.clear_downloads().unwrap();

        assert!(!first.downloads().exists());
        assert_eq!(
            fs::read_to_string(second.downloads().join("artifact.zip")).unwrap(),
            "second"
        );
    }
}
