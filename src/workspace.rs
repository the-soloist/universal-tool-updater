use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::{Builder, TempDir};

use crate::domain::Tool;

pub(crate) struct RunWorkspace {
    directory: TempDir,
    staging: TempDir,
    partials: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ToolWorkspace {
    downloads: PathBuf,
    unpacked: PathBuf,
    staging: PathBuf,
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

    pub(crate) fn clear_partials(&self) -> Result<()> {
        if self.partials.exists() {
            fs::remove_dir_all(&self.partials).with_context(|| {
                format!(
                    "cannot clear partial download directory {}",
                    self.partials.display()
                )
            })?;
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
    use std::path::PathBuf;

    use tempfile::tempdir;

    use crate::config::model::{ArtifactConfig, HookConfig, ReleaseConfig};
    use crate::domain::{ExistingPolicy, InputMode, InstallSpec, OutputMode, Tool};

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
            let first = run.prepare(&tool("first")).unwrap();
            let second = run.prepare(&tool("second")).unwrap();
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

    fn tool(id: &str) -> Tool {
        Tool {
            id: id.to_owned(),
            name: id.to_owned(),
            profile: "test".to_owned(),
            enabled: true,
            release: ReleaseConfig::Github {
                repository: "owner/repository".to_owned(),
                ignore_versions: Vec::new(),
            },
            artifacts: vec![ArtifactConfig::GithubAsset {
                pattern: "artifact".to_owned(),
            }],
            install: InstallSpec {
                destination: PathBuf::from(id),
                input: InputMode::Copy,
                existing: ExistingPolicy::Replace,
                save: OutputMode::Directory,
                strip_single_root: true,
                create_destination: true,
                archive_name: "{name}-{version}.7z".to_owned(),
                archive_password: None,
                executable: Vec::new(),
                symlinks: Vec::new(),
            },
            hooks: HookConfig::default(),
        }
    }
}
