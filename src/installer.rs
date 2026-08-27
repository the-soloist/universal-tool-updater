mod artifact;
mod filesystem;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use tempfile::Builder;

use artifact::ArtifactPreparer;
use filesystem::{
    apply_executable_bits, copy_tree, create_file_symlink, remove_path, single_directory_base,
};

use crate::archive::ArchiveService;
use crate::domain::{
    ArtifactConfig, DownloadedArtifact, ExistingPolicy, InputMode, OutputMode, Tool,
};
use crate::error::UpdaterError;
use crate::hooks::{HookContext, HookRunner, HookStage};
use crate::paths::is_portable_filename;
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

const RELEASE_VERSION_FILE: &str = ".version";

pub struct Installer<'a> {
    archive: &'a ArchiveService,
    hooks: &'a HookRunner,
    app_root: &'a Path,
    toolkit_root: &'a Path,
}

impl<'a> Installer<'a> {
    pub fn new(
        archive: &'a ArchiveService,
        hooks: &'a HookRunner,
        app_root: &'a Path,
        toolkit_root: &'a Path,
    ) -> Self {
        Self {
            archive,
            hooks,
            app_root,
            toolkit_root,
        }
    }

    pub(crate) fn install(
        &self,
        tool: &Tool,
        version: &str,
        artifacts: &[DownloadedArtifact],
        workspace: &ToolWorkspace,
        progress: &TaskProgress,
        compression_threads: usize,
    ) -> Result<()> {
        if artifacts.is_empty() {
            return Err(UpdaterError::Installation {
                tool: tool.id.clone(),
                message: "release contains no downloaded artifacts".to_owned(),
            }
            .into());
        }
        let destination = &tool.install.destination;
        let parent = destination
            .parent()
            .ok_or_else(|| UpdaterError::Installation {
                tool: tool.id.clone(),
                message: format!("destination {} has no parent", destination.display()),
            })?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create destination parent {}", parent.display()))?;

        let transaction = Builder::new()
            .prefix(&format!(".{}-staging-", tool.id))
            .tempdir_in(parent)
            .with_context(|| format!("cannot create staging directory in {}", parent.display()))?;
        let combined = transaction.path().join("content");
        fs::create_dir(&combined)?;
        let preparer = ArtifactPreparer::new(self.archive, workspace.unpacked());
        let output_mode = effective_output_mode(tool);

        if tool.install.existing == ExistingPolicy::Merge && destination.exists() {
            self.seed_existing(tool, destination, &combined, output_mode)?;
        }

        for (index, artifact) in artifacts.iter().enumerate() {
            progress.stage(if artifacts.len() > 1 {
                "extract artifact"
            } else {
                "extract"
            });
            let unpacked = preparer.prepare(tool, artifact, index)?;
            let hook_context = HookContext {
                app_root: self.app_root,
                toolkit_root: self.toolkit_root,
                downloads: workspace.downloads(),
                staging: Some(&unpacked),
                install: destination,
                version: Some(version),
            };
            self.hooks.run(
                &tool.hooks.after_unpack,
                HookStage::AfterUnpack,
                tool,
                &hook_context,
            )?;
            let source = if tool.install.strip_single_root {
                single_directory_base(&unpacked)?
            } else {
                unpacked
            };
            copy_tree(&source, &combined)?;
        }

        apply_executable_bits(tool, &combined)?;
        let release_version = stores_github_release_artifacts(tool)
            .then(|| transaction.path().join(RELEASE_VERSION_FILE));
        if let Some(path) = &release_version {
            fs::write(path, format!("{version}\n"))
                .with_context(|| format!("cannot stage release version for tool {}", tool.id))?;
        }
        let ready = match output_mode {
            OutputMode::Directory => combined,
            OutputMode::Archive => {
                progress.stage("compress");
                let output = transaction.path().join("archive-output");
                fs::create_dir(&output)?;
                let name = render_archive_name(&tool.install.archive_name, tool, version);
                if !is_portable_filename(Path::new(&name)) {
                    return Err(UpdaterError::Installation {
                        tool: tool.id.clone(),
                        message: format!(
                            "rendered archive name {name:?} is not a portable filename"
                        ),
                    }
                    .into());
                }
                self.archive.compress_7z_with_threads(
                    &combined,
                    &output.join(name),
                    compression_threads,
                )?;
                output
            }
        };
        progress.stage("install");
        self.commit(
            tool,
            version,
            &ready,
            release_version.as_deref(),
            workspace.downloads(),
        )?;
        Ok(())
    }

    fn seed_existing(
        &self,
        tool: &Tool,
        destination: &Path,
        combined: &Path,
        output_mode: OutputMode,
    ) -> Result<()> {
        match output_mode {
            OutputMode::Directory if tool.install.save == OutputMode::Archive => {
                if let Some(archive) = managed_archive_path(tool, destination)? {
                    self.archive.extract(&archive, combined, None)
                } else {
                    copy_tree(destination, combined)
                }
            }
            OutputMode::Directory => copy_tree(destination, combined),
            OutputMode::Archive => {
                if let Some(archive) = managed_archive_path(tool, destination)? {
                    self.archive.extract(&archive, combined, None)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn commit(
        &self,
        tool: &Tool,
        version: &str,
        ready: &Path,
        release_version: Option<&Path>,
        downloads: &Path,
    ) -> Result<()> {
        let destination = &tool.install.destination;
        let backup = backup_path(destination);
        if backup.exists() {
            remove_path(&backup)?;
        }
        let version_destination = release_version.map(|_| {
            destination
                .parent()
                .expect("an installation destination always has a parent")
                .join(RELEASE_VERSION_FILE)
        });
        let version_backup = version_destination.as_deref().map(backup_path);
        if let Some(backup) = &version_backup
            && backup.exists()
        {
            remove_path(backup)?;
        }
        let had_version = version_destination.as_deref().is_some_and(Path::exists);
        let had_destination = destination.exists();
        if had_destination {
            fs::rename(destination, &backup).with_context(|| {
                format!(
                    "cannot back up destination {} to {}",
                    destination.display(),
                    backup.display()
                )
            })?;
        }
        if let (Some(version_destination), Some(version_backup)) =
            (&version_destination, &version_backup)
            && had_version
            && let Err(error) = fs::rename(version_destination, version_backup)
        {
            if had_destination {
                let _ = fs::rename(&backup, destination);
            }
            return Err(error).with_context(|| {
                format!(
                    "cannot back up release version {} to {}",
                    version_destination.display(),
                    version_backup.display()
                )
            });
        }
        if let Err(error) = fs::rename(ready, destination) {
            if had_destination {
                let _ = fs::rename(&backup, destination);
            }
            if had_version {
                let _ = fs::rename(
                    version_backup
                        .as_deref()
                        .expect("version backup is present"),
                    version_destination
                        .as_deref()
                        .expect("version destination is present"),
                );
            }
            return Err(error).with_context(|| {
                format!("cannot commit installation to {}", destination.display())
            });
        }
        if let (Some(staged), Some(version_destination)) =
            (release_version, version_destination.as_deref())
            && let Err(error) = fs::rename(staged, version_destination)
        {
            let _ = remove_path(destination);
            if had_destination {
                let _ = fs::rename(&backup, destination);
            }
            if had_version {
                let _ = fs::rename(
                    version_backup
                        .as_deref()
                        .expect("version backup is present"),
                    version_destination,
                );
            }
            return Err(error).with_context(|| {
                format!(
                    "cannot commit release version to {}",
                    version_destination.display()
                )
            });
        }

        let post_commit = (|| -> Result<()> {
            self.install_symlinks(tool)?;
            let context = HookContext {
                app_root: self.app_root,
                toolkit_root: self.toolkit_root,
                downloads,
                staging: None,
                install: destination,
                version: Some(version),
            };
            self.hooks.run(
                &tool.hooks.after_install,
                HookStage::AfterInstall,
                tool,
                &context,
            )
        })();
        if let Err(error) = post_commit {
            let _ = remove_path(destination);
            if had_destination {
                let _ = fs::rename(&backup, destination);
            }
            if let Some(version_destination) = &version_destination {
                let _ = remove_path(version_destination);
                if had_version {
                    let _ = fs::rename(
                        version_backup
                            .as_deref()
                            .expect("version backup is present"),
                        version_destination,
                    );
                }
            }
            return Err(error);
        }
        if had_destination {
            remove_path(&backup)?;
        }
        if had_version {
            remove_path(
                version_backup
                    .as_deref()
                    .expect("version backup is present"),
            )?;
        }
        Ok(())
    }

    fn install_symlinks(&self, tool: &Tool) -> Result<()> {
        for link in &tool.install.symlinks {
            let source = tool.install.destination.join(&link.from);
            if !source.is_file() {
                return Err(UpdaterError::Installation {
                    tool: tool.id.clone(),
                    message: format!("symlink source {} is not a file", source.display()),
                }
                .into());
            }
            if let Some(parent) = link.to.parent() {
                fs::create_dir_all(parent)?;
            }
            if let Ok(metadata) = fs::symlink_metadata(&link.to) {
                if !metadata.file_type().is_symlink() {
                    return Err(UpdaterError::Installation {
                        tool: tool.id.clone(),
                        message: format!("refusing to replace non-symlink {}", link.to.display()),
                    }
                    .into());
                }
                fs::remove_file(&link.to)?;
            }
            create_file_symlink(&source, &link.to)?;
        }
        Ok(())
    }
}

fn effective_output_mode(tool: &Tool) -> OutputMode {
    if stores_github_release_artifacts(tool) {
        OutputMode::Directory
    } else {
        tool.install.save
    }
}

fn stores_github_release_artifacts(tool: &Tool) -> bool {
    tool.install.input == InputMode::Copy
        && tool.artifacts.iter().any(|artifact| {
            matches!(
                artifact,
                ArtifactConfig::GithubAsset { .. }
                    | ArtifactConfig::GithubAssets { .. }
                    | ArtifactConfig::GithubSource { .. }
            )
        })
}

fn backup_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .expect("an installation path always has a filename")
        .to_os_string();
    name.push(".utu-backup");
    path.with_file_name(name)
}

fn render_archive_name(template: &str, tool: &Tool, version: &str) -> String {
    template
        .replace("{id}", &tool.id)
        .replace("{name}", &tool.name)
        .replace("{version}", version)
}

fn managed_archive_path(tool: &Tool, destination: &Path) -> Result<Option<std::path::PathBuf>> {
    let pattern = managed_archive_pattern(tool);
    let mut archives = Vec::new();
    for entry in fs::read_dir(destination)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| pattern.is_match(name)) {
            archives.push(entry.path());
        }
    }
    archives.sort();
    match archives.len() {
        0 => Ok(None),
        1 => Ok(archives.pop()),
        _ => Err(UpdaterError::Installation {
            tool: tool.id.clone(),
            message: format!(
                "destination {} contains multiple updater-managed archives",
                destination.display()
            ),
        }
        .into()),
    }
}

fn managed_archive_pattern(tool: &Tool) -> Regex {
    let placeholders = ["{id}", "{name}", "{version}"];
    let mut remaining = tool.install.archive_name.as_str();
    let mut pattern = String::from("^");
    while let Some((index, placeholder)) = placeholders
        .iter()
        .filter_map(|placeholder| {
            remaining
                .find(placeholder)
                .map(|index| (index, *placeholder))
        })
        .min_by_key(|(index, _)| *index)
    {
        pattern.push_str(&regex::escape(&remaining[..index]));
        match placeholder {
            "{id}" => pattern.push_str(&regex::escape(&tool.id)),
            "{name}" => pattern.push_str(&regex::escape(&tool.name)),
            "{version}" => pattern.push_str(".+"),
            _ => unreachable!("placeholder comes from the static list"),
        }
        remaining = &remaining[index + placeholder.len()..];
    }
    pattern.push_str(&regex::escape(remaining));
    pattern.push('$');
    Regex::new(&pattern).expect("archive template is converted to a valid regular expression")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::archive::ArchiveService;
    use crate::config::model::{
        ArtifactConfig, ExistingPolicy, HookAction, HookConfig, InputMode, OutputMode,
        ReleaseConfig,
    };
    use crate::domain::{DownloadedArtifact, InstallSpec, Tool};
    use crate::hooks::HookRunner;
    use crate::progress::ProgressManager;
    use crate::workspace::RunWorkspace;

    use super::{Installer, managed_archive_path, managed_archive_pattern};

    #[test]
    fn keeps_github_copy_artifacts_uncompressed_and_records_release_version() {
        let directory = tempdir().unwrap();
        let toolkit = directory.path().join("Toolkit");
        let downloads = toolkit.join("updates");
        let destination = toolkit.join("Demo/release");
        fs::create_dir_all(&downloads).unwrap();

        let old_content = directory.path().join("old-content");
        fs::create_dir(&old_content).unwrap();
        fs::write(old_content.join("old.bin"), "old").unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.parent().unwrap().join(".version"), "v1\n").unwrap();

        let archive_service = ArchiveService;
        archive_service
            .compress_7z(&old_content, &destination.join("Demo-v1.7z"))
            .unwrap();

        let asset = downloads.join("demo.zip");
        let assets = downloads.join("demo.xz");
        let source = downloads.join("demo-source.tar.gz");
        fs::write(&asset, "asset").unwrap();
        fs::write(&assets, "assets").unwrap();
        fs::write(&source, "source").unwrap();

        let tool = Tool {
            id: "demo".to_owned(),
            name: "Demo".to_owned(),
            profile: "test".to_owned(),
            enabled: true,
            release: ReleaseConfig::Github {
                repository: "owner/demo".to_owned(),
                ignore_versions: Vec::new(),
            },
            artifacts: vec![
                ArtifactConfig::GithubAsset {
                    pattern: "demo.zip".to_owned(),
                },
                ArtifactConfig::GithubAssets {
                    pattern: "demo.xz".to_owned(),
                },
                ArtifactConfig::GithubSource {
                    format: "tar.gz".to_owned(),
                },
            ],
            install: InstallSpec {
                destination: destination.clone(),
                input: InputMode::Copy,
                existing: ExistingPolicy::Merge,
                save: OutputMode::Archive,
                strip_single_root: true,
                create_destination: true,
                archive_name: "{name}-{version}.7z".to_owned(),
                archive_password: None,
                executable: Vec::new(),
                symlinks: Vec::new(),
            },
            hooks: HookConfig::default(),
        };
        assert_eq!(super::effective_output_mode(&tool), OutputMode::Directory);
        let mut extracted_tool = tool.clone();
        extracted_tool.install.input = InputMode::Extract;
        assert_eq!(
            super::effective_output_mode(&extracted_tool),
            OutputMode::Archive
        );
        let mut mixed_tool = tool.clone();
        mixed_tool.artifacts.push(ArtifactConfig::DirectUrl {
            url: "https://example.com/demo.bin".to_owned(),
        });
        assert_eq!(
            super::effective_output_mode(&mixed_tool),
            OutputMode::Directory
        );
        let hook_runner = HookRunner;
        let installer = Installer::new(&archive_service, &hook_runner, directory.path(), &toolkit);
        let run = RunWorkspace::create(&downloads).unwrap();
        let workspace = run.prepare(&tool).unwrap();
        let progress = ProgressManager::new(false, 1);
        let task_progress = progress.task(&tool.profile, &tool.name);

        installer
            .install(
                &tool,
                "v2",
                &[
                    DownloadedArtifact { path: asset },
                    DownloadedArtifact { path: assets },
                    DownloadedArtifact { path: source },
                ],
                &workspace,
                &task_progress,
                1,
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("old.bin")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(destination.join("demo.zip")).unwrap(),
            "asset"
        );
        assert_eq!(
            fs::read_to_string(destination.join("demo.xz")).unwrap(),
            "assets"
        );
        assert_eq!(
            fs::read_to_string(destination.join("demo-source.tar.gz")).unwrap(),
            "source"
        );
        assert_eq!(
            fs::read_to_string(destination.parent().unwrap().join(".version")).unwrap(),
            "v2\n"
        );
        assert!(!destination.join(".version").exists());
        assert!(!destination.join("Demo-v1.7z").exists());
        assert!(!destination.join("Demo-v2.7z").exists());
        assert!(
            !destination
                .parent()
                .unwrap()
                .join(".version.utu-backup")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn restores_previous_installation_when_post_install_hook_fails() {
        let directory = tempdir().unwrap();
        let toolkit = directory.path().join("Toolkit");
        let downloads = toolkit.join("updates");
        let tool_root = toolkit.join("Demo");
        let destination = tool_root.join("release");
        fs::create_dir_all(&downloads).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("old.txt"), "old").unwrap();
        fs::write(tool_root.join(".version"), "v1\n").unwrap();
        let artifact = downloads.join("new.bin");
        fs::write(&artifact, "new").unwrap();
        let tool = Tool {
            id: "demo".to_owned(),
            name: "Demo".to_owned(),
            profile: "test".to_owned(),
            enabled: true,
            release: ReleaseConfig::Github {
                repository: "owner/demo".to_owned(),
                ignore_versions: Vec::new(),
            },
            artifacts: vec![ArtifactConfig::GithubAsset {
                pattern: "demo".to_owned(),
            }],
            install: InstallSpec {
                destination: destination.clone(),
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
            hooks: HookConfig {
                after_install: vec![HookAction::Rename {
                    from: "new.bin".to_owned(),
                    to: "renamed.bin".into(),
                }],
                ..HookConfig::default()
            },
        };
        let archive_service = ArchiveService;
        let hook_runner = HookRunner;
        let installer = Installer::new(&archive_service, &hook_runner, directory.path(), &toolkit);
        let run = RunWorkspace::create(&downloads).unwrap();
        let workspace = run.prepare(&tool).unwrap();
        let progress = ProgressManager::new(false, 1);
        let task_progress = progress.task(&tool.profile, &tool.name);

        let result = installer.install(
            &tool,
            "v2",
            &[DownloadedArtifact { path: artifact }],
            &workspace,
            &task_progress,
            1,
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(destination.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!destination.join("new.bin").exists());
        assert_eq!(
            fs::read_to_string(tool_root.join(".version")).unwrap(),
            "v1\n"
        );
        assert!(!tool_root.join(".version.utu-backup").exists());
    }

    #[test]
    fn recognizes_only_updater_managed_archives_for_merging() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("third-party.7z"), "not an updater archive").unwrap();
        let tool = Tool {
            id: "demo".to_owned(),
            name: "Demo".to_owned(),
            profile: "test".to_owned(),
            enabled: true,
            release: ReleaseConfig::Github {
                repository: "owner/demo".to_owned(),
                ignore_versions: Vec::new(),
            },
            artifacts: Vec::new(),
            install: InstallSpec {
                destination: destination.clone(),
                input: InputMode::Extract,
                existing: ExistingPolicy::Merge,
                save: OutputMode::Archive,
                strip_single_root: true,
                create_destination: true,
                archive_name: "{name}#{version}.7z".to_owned(),
                archive_password: None,
                executable: Vec::new(),
                symlinks: Vec::new(),
            },
            hooks: HookConfig::default(),
        };

        assert!(managed_archive_path(&tool, &destination).unwrap().is_none());
        fs::write(destination.join("Demo#1.2.3.7z"), "managed").unwrap();
        assert_eq!(
            managed_archive_path(&tool, &destination).unwrap(),
            Some(destination.join("Demo#1.2.3.7z"))
        );
        let pattern = managed_archive_pattern(&tool);
        assert!(pattern.is_match("Demo#1.2.3.7z"));
        assert!(!pattern.is_match("third-party.7z"));
    }
}
