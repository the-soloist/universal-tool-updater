mod artifact;
mod filesystem;
mod output;
mod transaction;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use artifact::ArtifactPreparer;
use filesystem::{apply_executable_bits, copy_tree, single_directory_base};
use output::{effective_mode, managed_archive_path, render_archive_name};
use transaction::{CommitRequest, CommitSource, commit, same_filesystem};

pub(crate) use output::installation_matches;

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

/// Inputs shared by every staging phase of a single installation.
struct InstallJob<'a> {
    tool: &'a Tool,
    version: &'a str,
    artifacts: &'a [DownloadedArtifact],
    workspace: &'a ToolWorkspace,
    progress: &'a TaskProgress,
    transaction: &'a Path,
    parent: &'a Path,
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

        let transaction = workspace.staging();
        let job = InstallJob {
            tool,
            version,
            artifacts,
            workspace,
            progress,
            transaction,
            parent,
        };
        let combined = transaction.join("content");
        let preparer = ArtifactPreparer::new(self.archive, workspace.unpacked());
        let output_mode = effective_mode(tool);

        self.stage_content(&job, &preparer, &combined, output_mode)?;
        let external_version = self.stage_version_marker(&job, &combined, output_mode)?;
        let ready = self.package_archive(&job, &combined, output_mode, compression_threads)?;
        self.commit_staged(&job, &ready, external_version.as_deref())?;
        Ok(())
    }

    fn stage_content(
        &self,
        job: &InstallJob<'_>,
        preparer: &ArtifactPreparer<'_>,
        combined: &Path,
        output_mode: OutputMode,
    ) -> Result<()> {
        let tool = job.tool;
        let destination = &tool.install.destination;
        fs::create_dir(combined)?;

        if tool.install.existing == ExistingPolicy::Merge && destination.exists() {
            self.seed_existing(tool, destination, combined, output_mode)?;
        }

        for (index, artifact) in job.artifacts.iter().enumerate() {
            job.progress.stage(if job.artifacts.len() > 1 {
                "extract artifact"
            } else {
                "extract"
            });
            let unpacked = preparer.prepare(tool, artifact, index)?;
            let hook_context = HookContext {
                app_root: self.app_root,
                toolkit_root: self.toolkit_root,
                downloads: job.workspace.downloads(),
                staging: Some(&unpacked),
                install: destination,
                version: Some(job.version),
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
            // unpacked -> combined stays inside the run workspace, so hard
            // links are safe: their inodes never outlive the run directory.
            copy_tree(&source, combined, true)?;
        }

        apply_executable_bits(tool, combined)
    }

    fn stage_version_marker(
        &self,
        job: &InstallJob<'_>,
        combined: &Path,
        output_mode: OutputMode,
    ) -> Result<Option<std::path::PathBuf>> {
        let external_version = (output_mode == OutputMode::Directory
            && job.tool.install.input == InputMode::Copy)
            .then(|| job.transaction.join(VERSION_FILE));
        if let Some(path) = &external_version {
            fs::write(path, format!("{}\n", job.version))
                .with_context(|| format!("cannot stage version marker for tool {}", job.tool.id))?;
        } else if output_mode == OutputMode::Directory {
            fs::write(combined.join(VERSION_FILE), format!("{}\n", job.version))
                .with_context(|| format!("cannot stage version marker for tool {}", job.tool.id))?;
        }
        Ok(external_version)
    }

    fn package_archive(
        &self,
        job: &InstallJob<'_>,
        combined: &Path,
        output_mode: OutputMode,
        compression_threads: usize,
    ) -> Result<std::path::PathBuf> {
        match output_mode {
            OutputMode::Directory => Ok(combined.to_path_buf()),
            OutputMode::Archive => {
                job.progress.stage("compress");
                let output = job.transaction.join("archive-output");
                fs::create_dir(&output)?;
                let name =
                    render_archive_name(&job.tool.install.archive_name, job.tool, job.version);
                if !is_portable_filename(Path::new(&name)) {
                    return Err(UpdaterError::Installation {
                        tool: job.tool.id.clone(),
                        message: format!(
                            "rendered archive name {name:?} is not a portable filename"
                        ),
                    }
                    .into());
                }
                self.archive.compress_7z_with_threads(
                    combined,
                    &output.join(name),
                    compression_threads,
                )?;
                Ok(output)
            }
        }
    }

    fn commit_staged(
        &self,
        job: &InstallJob<'_>,
        ready: &Path,
        external_version: Option<&Path>,
    ) -> Result<()> {
        let tool = job.tool;
        let direct_commit = same_filesystem(job.transaction, job.parent)?;
        let commit_source = if direct_commit {
            CommitSource::direct(ready, external_version)
        } else {
            job.progress.stage("transfer");
            CommitSource::copy_next_to_destination(tool, ready, external_version, job.parent)?
        };
        tracing::debug!(
            tool = %tool.id,
            staging = %job.transaction.display(),
            destination = %tool.install.destination.display(),
            direct_commit,
            "prepared installation transaction"
        );
        job.progress.stage("install");
        commit(CommitRequest {
            tool,
            version: job.version,
            ready: &commit_source.ready,
            external_version: commit_source.external_version.as_deref(),
            downloads: job.workspace.downloads(),
            hooks: self.hooks,
            app_root: self.app_root,
            toolkit_root: self.toolkit_root,
        })
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
                    self.archive.extract_for_tool(
                        &tool.id,
                        tool.install.allow_symlinks_in_archive,
                        &archive,
                        combined,
                        None,
                    )
                } else {
                    // destination -> combined must stay a real copy: a hard
                    // link here would let the merge overwrite truncate
                    // through the shared inode and poison the rollback
                    // backup with new content.
                    copy_tree(destination, combined, false)
                }
            }
            OutputMode::Directory => copy_tree(destination, combined, false),
            OutputMode::Archive => {
                if let Some(archive) = managed_archive_path(tool, destination)? {
                    self.archive.extract_for_tool(
                        &tool.id,
                        tool.install.allow_symlinks_in_archive,
                        &archive,
                        combined,
                        None,
                    )
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
