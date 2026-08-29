use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::archive::ArchiveService;
use crate::config::AppConfig;
use crate::domain::{Tool, UpdateResult, UpdateStatus};
use crate::downloader::Downloader;
use crate::hooks::{HookContext, HookRunner, HookStage};
use crate::installer::{Installer, installation_matches};
use crate::progress::{ProgressManager, TaskProgress};
use crate::resolver::Resolver;
use crate::state::StateStore;
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
    let resolver = Resolver::new(&config.network)?;
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
    let progress = ProgressManager::new(
        config.network.progress && !options.no_progress && !options.verbose,
        selected.len(),
    );
    let session = UpdateSession {
        config,
        resolver: &resolver,
        downloader: &downloader,
        installer: &installer,
        hooks: &hooks,
        state_versions: &state_versions,
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
        for _ in 0..workers {
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
                    let task_progress = session.progress.task(&tool.profile, &tool.name);
                    let result = session
                        .update_one(tool, &task_progress)
                        .unwrap_or_else(|error| UpdateResult {
                            tool_id: tool.id.clone(),
                            status: UpdateStatus::Failed,
                            version: None,
                            message: format!("{error:#}"),
                        });
                    if sender
                        .send(TaskOutcome {
                            index,
                            result,
                            progress: task_progress,
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
            if outcome.result.status == UpdateStatus::Updated {
                let version = outcome
                    .result
                    .version
                    .as_deref()
                    .expect("updated results always contain a version");
                if let Err(error) = state.record(&outcome.result.tool_id, version) {
                    outcome.result.status = UpdateStatus::Failed;
                    outcome.result.message =
                        format!("update installed but state could not be recorded: {error:#}");
                }
            }
            progress.complete(&outcome.progress);
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
    installer: &'a Installer<'a>,
    hooks: &'a HookRunner,
    state_versions: &'a BTreeMap<String, String>,
    workspace: Option<&'a RunWorkspace>,
    progress: &'a ProgressManager,
    compression_threads: usize,
    options: UpdateOptions,
}

impl UpdateSession<'_> {
    fn update_one(&self, tool: &Tool, progress: &TaskProgress) -> Result<UpdateResult> {
        if !tool.enabled {
            return Ok(result(tool, UpdateStatus::Skipped, None, "disabled"));
        }
        if !tool.install.destination.exists()
            && !tool.install.create_destination
            && !self.options.create_missing
        {
            return Ok(result(
                tool,
                UpdateStatus::Skipped,
                None,
                "destination does not exist",
            ));
        }

        progress.stage("resolve");
        let release = self.resolver.resolve(tool)?;
        if !self.options.force
            && self.state_versions.get(&tool.id).map(String::as_str)
                == Some(release.version.as_str())
            && installation_matches(tool, &release.version)
        {
            return Ok(result(
                tool,
                UpdateStatus::Current,
                Some(&release.version),
                "already current",
            ));
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
            return Ok(result(
                tool,
                UpdateStatus::Planned,
                Some(&release.version),
                "dry run",
            ));
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
            self.compression_threads,
        )?;
        workspace.clear_partials()?;
        Ok(result(
            tool,
            UpdateStatus::Updated,
            Some(&release.version),
            "update complete",
        ))
    }
}

struct TaskOutcome {
    index: usize,
    result: UpdateResult,
    progress: TaskProgress,
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
