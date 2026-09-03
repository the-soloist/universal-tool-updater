mod artifact;
mod filesystem;
mod output;
mod transaction;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use artifact::ArtifactPreparer;
use filesystem::{apply_executable_bits, copy_tree, single_directory_base, verify_staged_links};
use output::{effective_mode, managed_archive_path, render_archive_name};
use transaction::{CommitRequest, CommitSource, commit, same_filesystem};

pub(crate) use output::{installation_matches, installed_archive_path, installed_archive_state};

use crate::archive::ArchiveService;
use crate::domain::{
    DownloadedArtifact, ExistingPolicy, InputMode, OutputMode, Tool, VERSION_FILE,
};
use crate::error::UpdaterError;
use crate::hooks::{HookContext, HookRunner, HookStage};
use crate::paths::is_portable_filename;
use crate::progress::TaskProgress;
use crate::workspace::ToolWorkspace;

pub struct Installer<'a> {
    archive: &'a ArchiveService,
    hooks: &'a HookRunner,
    app_root: &'a Path,
    toolkit_root: &'a Path,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExistingArchiveStatus {
    #[default]
    Unchecked,
    Invalid,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InstallOptions {
    compression_threads: usize,
    existing_archive: ExistingArchiveStatus,
}

impl InstallOptions {
    pub(crate) fn new(compression_threads: usize, existing_archive: ExistingArchiveStatus) -> Self {
        Self {
            compression_threads,
            existing_archive,
        }
    }
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
        options: InstallOptions,
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

        let transaction = workspace.staging();
        let combined = transaction.join("content");
        fs::create_dir(&combined)?;
        let preparer = ArtifactPreparer::new(self.archive, workspace.unpacked());
        let output_mode = effective_mode(tool);

        if tool.install.existing == ExistingPolicy::Merge && destination.exists() {
            self.seed_existing(
                tool,
                destination,
                &combined,
                output_mode,
                options.existing_archive,
            )?;
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

        // 提升与合并改变了链接的相对边界；opt-in 放行过链接时，
        // 以最终组合目录为根复验，防止解压时界内的目标在此越界。
        if tool.install.allow_symlinks_in_archive {
            verify_staged_links(&combined).map_err(|error| UpdaterError::Installation {
                tool: tool.id.clone(),
                message: error.to_string(),
            })?;
        }

        apply_executable_bits(tool, &combined)?;
        // Copy 输入的目录输出把版本标记放在目标目录旁，避免把元数据混入用户文件。
        let external_version = (output_mode == OutputMode::Directory
            && tool.install.input == InputMode::Copy)
            .then(|| transaction.join(VERSION_FILE));
        if let Some(path) = &external_version {
            fs::write(path, format!("{version}\n"))
                .with_context(|| format!("cannot stage version marker for tool {}", tool.id))?;
        } else if output_mode == OutputMode::Directory {
            fs::write(combined.join(VERSION_FILE), format!("{version}\n"))
                .with_context(|| format!("cannot stage version marker for tool {}", tool.id))?;
        }
        let ready = match output_mode {
            OutputMode::Directory => combined,
            OutputMode::Archive => {
                progress.stage("compress");
                let output = transaction.join("archive-output");
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
                    options.compression_threads,
                )?;
                output
            }
        };
        let direct_commit = same_filesystem(transaction, parent)?;
        let commit_source = if direct_commit {
            CommitSource::direct(&ready, external_version.as_deref())
        } else {
            progress.stage("transfer");
            CommitSource::copy_next_to_destination(
                tool,
                &ready,
                external_version.as_deref(),
                parent,
            )?
        };
        tracing::debug!(
            tool = %tool.id,
            staging = %transaction.display(),
            destination = %destination.display(),
            direct_commit,
            "prepared installation transaction"
        );
        progress.stage("install");
        commit(CommitRequest {
            tool,
            version,
            ready: &commit_source.ready,
            external_version: commit_source.external_version.as_deref(),
            downloads: workspace.downloads(),
            hooks: self.hooks,
            app_root: self.app_root,
            toolkit_root: self.toolkit_root,
        })?;
        Ok(())
    }

    fn seed_existing(
        &self,
        tool: &Tool,
        destination: &Path,
        combined: &Path,
        output_mode: OutputMode,
        existing_archive: ExistingArchiveStatus,
    ) -> Result<()> {
        let archives_existing =
            output_mode == OutputMode::Archive || tool.install.save == OutputMode::Archive;
        if archives_existing && let Some(archive) = managed_archive_path(tool, destination)? {
            if existing_archive == ExistingArchiveStatus::Invalid {
                return Ok(());
            }
            match self.archive.verify_7z(&archive) {
                Ok(()) => {}
                Err(error) if error.is_invalid() => {
                    // 已损坏的合并基线无法可靠恢复；以发布产物重建，避免修复流程再次解压同一坏包。
                    tracing::warn!(
                        tool = %tool.id,
                        path = %archive.display(),
                        error = %error,
                        "existing archive is invalid; rebuilding without its contents"
                    );
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
            return self.archive.extract(&archive, combined, None);
        }
        if output_mode == OutputMode::Directory {
            copy_tree(destination, combined)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
