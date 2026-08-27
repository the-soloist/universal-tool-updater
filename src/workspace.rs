use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::{Builder, TempDir};

use crate::domain::Tool;

pub(crate) struct RunWorkspace {
    directory: TempDir,
}

#[derive(Debug)]
pub(crate) struct ToolWorkspace {
    downloads: PathBuf,
    unpacked: PathBuf,
}

impl RunWorkspace {
    pub(crate) fn create(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("cannot create update directory {}", root.display()))?;
        let directory = Builder::new()
            .prefix("run-")
            .tempdir_in(root)
            .with_context(|| format!("cannot create run directory in {}", root.display()))?;
        Ok(Self { directory })
    }

    pub(crate) fn prepare(&self, tool: &Tool) -> Result<ToolWorkspace> {
        let root = self.directory.path().join(&tool.id);
        let downloads = root.join("downloads");
        let unpacked = root.join("unpacked");
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
        Ok(ToolWorkspace {
            downloads,
            unpacked,
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
    fn tool_directories_are_isolated_and_run_directory_is_temporary() {
        let root = tempdir().unwrap();
        let run_path;
        {
            let run = RunWorkspace::create(root.path()).unwrap();
            let first = run.prepare(&tool("first")).unwrap();
            let second = run.prepare(&tool("second")).unwrap();
            run_path = first
                .downloads()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();

            fs::write(first.downloads().join("artifact.zip"), "first").unwrap();
            fs::write(second.downloads().join("artifact.zip"), "second").unwrap();

            assert_eq!(
                fs::read_to_string(first.downloads().join("artifact.zip")).unwrap(),
                "first"
            );
            assert_eq!(
                fs::read_to_string(second.downloads().join("artifact.zip")).unwrap(),
                "second"
            );
            assert_ne!(first.downloads().parent(), second.downloads().parent());
        }
        assert!(!run_path.exists());
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
