use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::archive::ArchiveService;
use crate::config::AppConfig;
use crate::domain::{ReleaseConfig, Tool, UpdateResult, UpdateStatus};
use crate::downloader::Downloader;
use crate::hooks::{HookContext, HookRunner, HookStage};
use crate::installer::{
    ExistingArchiveStatus, InstallOptions, Installer, installation_matches, installed_archive_path,
    installed_archive_state,
};
use crate::progress::{ProgressManager, TaskProgress};
use crate::resolver::Resolver;
use crate::state::{ArchiveState, StateStore};
use crate::workspace::RunWorkspace;

use super::report::print_summary;
use super::selection::{select_tools, validate_profiles};

#[derive(Debug, Clone, Copy)]
pub(super) struct UpdateOptions {
    pub(super) force: bool,
    pub(super) create_missing: bool,
    pub(super) dry_run: bool,
    pub(super) no_progress: bool,
    pub(super) verbose: bool,
    pub(super) jobs: Option<usize>,
}

pub(super) fn update_tools(
    config: &AppConfig,
    requested: &[String],
    profiles: &[String],
    options: UpdateOptions,
) -> Result<()> {
    validate_profiles(config, profiles)?;
    let selected = select_tools(config, requested, profiles)?;
    if selected.is_empty() {
        bail!("no tools matched the selection");
    }

    if !options.dry_run {
        fs::create_dir_all(&config.paths.toolkit_root).with_context(|| {
            format!(
                "cannot create toolkit root {}",
                config.paths.toolkit_root.display()
            )
        })?;
    }
    let mut state = StateStore::load(&config.paths.state)?;
    let state_versions = selected
        .iter()
        .filter_map(|tool| {
            state
                .version(&tool.id)
                .map(|version| (tool.id.clone(), version.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    // 归档输出还需记录文件身份，避免仅凭版本号跳过已被替换的归档。
    let state_archives = selected
        .iter()
        .filter_map(|tool| {
            state
                .archive(&tool.id)
                .map(|archive| (tool.id.clone(), archive.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let resolver = Resolver::new(&config.network, config.allow_insecure_transports)?;
    let downloader = Downloader::new(resolver.client().clone());
    let workspace = if options.dry_run {
        None
    } else {
        Some(RunWorkspace::create(
            &config.paths.downloads,
            &config.paths.staging,
        )?)
    };
    let archive = ArchiveService;
    let hooks = HookRunner;
    let installer = Installer::new(
        &archive,
        &hooks,
        &config.app_root,
        &config.paths.toolkit_root,
    );
    let workers = effective_jobs(options.jobs, config.network.jobs, selected.len());
    let compression_threads = compression_threads(workers);
    let progress = ProgressManager::new_with_workers(
        config.network.progress && !options.no_progress && !options.verbose,
        selected.len(),
        workers,
    );
    let session = UpdateSession {
        config,
        resolver: &resolver,
        downloader: &downloader,
        archive: &archive,
        installer: &installer,
        hooks: &hooks,
        state_versions: &state_versions,
        state_archives: &state_archives,
        workspace: workspace.as_ref(),
        progress: &progress,
        compression_threads,
        options,
    };
    let mut ordered_results = (0..selected.len())
        .map(|_| None)
        .collect::<Vec<Option<UpdateResult>>>();
    let next = AtomicUsize::new(0);
    let parallel_result = thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = mpsc::channel::<TaskOutcome>();
        for slot in 0..workers {
            let sender = sender.clone();
            let selected = &selected;
            let session = &session;
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(tool) = selected.get(index).copied() else {
                        break;
                    };
                    let task_progress =
                        session
                            .progress
                            .task_in_slot(slot, &tool.profile, &tool.name);
                    let update = session
                        .update_one(tool, &task_progress)
                        .unwrap_or_else(|error| ToolUpdate {
                            archive: None,
                            result: UpdateResult {
                                tool_id: tool.id.clone(),
                                status: UpdateStatus::Failed,
                                version: None,
                                message: format!("{error:#}"),
                            },
                        });
                    if sender
                        .send(TaskOutcome {
                            index,
                            result: update.result,
                            archive: update.archive,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(sender);

        for _ in 0..selected.len() {
            let mut outcome = receiver
                .recv()
                .context("update worker stopped before returning every tool result")?;
            // 仅在安装完成或归档已验证后更新状态，失败结果不能污染下次更新判断。
            let record = match outcome.result.status {
                UpdateStatus::Updated => {
                    let version = outcome
                        .result
                        .version
                        .as_deref()
                        .expect("updated results always contain a version");
                    let tool = selected[outcome.index];
                    installed_archive_state(tool, version).and_then(|archive| {
                        state.record_installation(&outcome.result.tool_id, version, archive)
                    })
                }
                UpdateStatus::Current => outcome.archive.take().map_or(Ok(()), |archive| {
                    state.record_archive(&outcome.result.tool_id, archive)
                }),
                _ => Ok(()),
            };
            if let Err(error) = record {
                let message = if outcome.result.status == UpdateStatus::Updated {
                    "update installed but installation state could not be recorded"
                } else {
                    "installed archive verified but its state could not be recorded"
                };
                outcome.result.status = UpdateStatus::Failed;
                outcome.result.message = format!("{message}: {error:#}");
            }
            progress.complete();
            ordered_results[outcome.index] = Some(outcome.result);
        }
        Ok(())
    });
    progress.finish();
    parallel_result?;
    let results = ordered_results
        .into_iter()
        .map(|result| result.expect("every selected tool returns one result"))
        .collect::<Vec<_>>();
    print_summary(&results);
    if results
        .iter()
        .any(|result| result.status == UpdateStatus::Failed)
    {
        bail!("one or more tools failed to update");
    }
    Ok(())
}

struct UpdateSession<'a> {
    config: &'a AppConfig,
    resolver: &'a Resolver,
    downloader: &'a Downloader,
    archive: &'a ArchiveService,
    installer: &'a Installer<'a>,
    hooks: &'a HookRunner,
    state_versions: &'a BTreeMap<String, String>,
    state_archives: &'a BTreeMap<String, ArchiveState>,
    workspace: Option<&'a RunWorkspace>,
    progress: &'a ProgressManager,
    compression_threads: usize,
    options: UpdateOptions,
}

impl UpdateSession<'_> {
    fn update_one(&self, tool: &Tool, progress: &TaskProgress) -> Result<ToolUpdate> {
        let mut existing_archive = ExistingArchiveStatus::Unchecked;
        if !tool.enabled {
            return Ok(ToolUpdate::new(result(
                tool,
                UpdateStatus::Skipped,
                None,
                "disabled",
            )));
        }
        if matches!(tool.release, ReleaseConfig::Manual {}) {
            return Ok(ToolUpdate::new(result(
                tool,
                UpdateStatus::Skipped,
                None,
                "maintained manually; not auto-updated",
            )));
        }
        if !tool.install.destination.exists()
            && !tool.install.create_destination
            && !self.options.create_missing
        {
            return Ok(ToolUpdate::new(result(
                tool,
                UpdateStatus::Skipped,
                None,
                "destination does not exist",
            )));
        }

        progress.stage("resolve");
        let release = self.resolver.resolve(tool)?;
        let recorded_version_matches =
            self.state_versions.get(&tool.id).map(String::as_str) == Some(release.version.as_str());
        if !self.options.force && recorded_version_matches {
            if installation_matches(tool, &release.version, self.state_archives.get(&tool.id)) {
                return Ok(ToolUpdate::new(result(
                    tool,
                    UpdateStatus::Current,
                    Some(&release.version),
                    "already current",
                )));
            }
            // 旧状态没有归档身份，或文件身份已变化时，仅做一次完整校验并刷新凭据。
            if let Some(path) = installed_archive_path(tool, &release.version) {
                progress.stage("verify");
                match self.archive.verify_7z(&path) {
                    Ok(()) => {
                        let archive = installed_archive_state(tool, &release.version)?
                            .expect("archive output always produces archive state");
                        return Ok(ToolUpdate {
                            archive: Some(archive),
                            result: result(
                                tool,
                                UpdateStatus::Current,
                                Some(&release.version),
                                "already current",
                            ),
                        });
                    }
                    Err(error) if error.is_invalid() => {
                        existing_archive = ExistingArchiveStatus::Invalid;
                        tracing::warn!(
                            tool = %tool.id,
                            path = %path.display(),
                            error = %error,
                            "installed archive is invalid; rebuilding without its contents"
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if self.options.dry_run {
            let urls = release
                .artifacts
                .iter()
                .map(|artifact| artifact.url.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::debug!(
                tool = %tool.id,
                version = %release.version,
                destination = %tool.install.destination.display(),
                artifacts = %urls,
                "planned update"
            );
            return Ok(ToolUpdate::new(result(
                tool,
                UpdateStatus::Planned,
                Some(&release.version),
                "dry run",
            )));
        }

        let workspace = self
            .workspace
            .context("run workspace is unavailable for a real update")?
            .prepare(tool)?;
        progress.stage("prepare");
        let before_context = HookContext {
            app_root: &self.config.app_root,
            toolkit_root: &self.config.paths.toolkit_root,
            downloads: workspace.downloads(),
            staging: None,
            install: &tool.install.destination,
            version: Some(&release.version),
        };
        self.hooks.run(
            &tool.hooks.before_update,
            HookStage::BeforeUpdate,
            tool,
            &before_context,
        )?;

        let downloaded = release
            .artifacts
            .iter()
            .enumerate()
            .map(|(index, artifact)| {
                self.downloader.download(
                    tool,
                    &release.version,
                    artifact,
                    &workspace,
                    (index, release.artifacts.len()),
                    progress,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        self.installer.install(
            tool,
            &release.version,
            &downloaded,
            &workspace,
            progress,
            InstallOptions::new(self.compression_threads, existing_archive),
        )?;
        workspace.clear_partials()?;
        workspace.clear_downloads()?;
        Ok(ToolUpdate::new(result(
            tool,
            UpdateStatus::Updated,
            Some(&release.version),
            "update complete",
        )))
    }
}

struct TaskOutcome {
    index: usize,
    result: UpdateResult,
    archive: Option<ArchiveState>,
}

struct ToolUpdate {
    result: UpdateResult,
    archive: Option<ArchiveState>,
}

impl ToolUpdate {
    fn new(result: UpdateResult) -> Self {
        Self {
            result,
            archive: None,
        }
    }
}

fn effective_jobs(command_line: Option<usize>, configured: usize, tools: usize) -> usize {
    command_line.unwrap_or(configured).max(1).min(tools.max(1))
}

fn compression_threads(workers: usize) -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .checked_div(workers.max(1))
        .unwrap_or(1)
        .max(1)
}

fn result(tool: &Tool, status: UpdateStatus, version: Option<&str>, message: &str) -> UpdateResult {
    UpdateResult {
        tool_id: tool.id.clone(),
        status,
        version: version.map(ToOwned::to_owned),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{compression_threads, effective_jobs};

    #[test]
    fn bounds_workers_by_configuration_and_selection_size() {
        assert_eq!(effective_jobs(Some(8), 4, 3), 3);
        assert_eq!(effective_jobs(Some(1), 4, 3), 1);
        assert_eq!(effective_jobs(None, 4, 3), 3);
        assert_eq!(effective_jobs(Some(2), 6, 8), 2);
        assert_eq!(effective_jobs(None, 6, 8), 6);
    }

    #[test]
    fn always_assigns_at_least_one_compression_thread() {
        assert!(compression_threads(usize::MAX) >= 1);
    }
}
