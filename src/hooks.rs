mod native;
mod python;

use std::path::Path;

use anyhow::Result;

use self::python::PythonCache;
use crate::domain::{HookAction, Tool};
use crate::error::UpdaterError;

#[derive(Debug, Clone, Copy)]
pub enum HookStage {
    BeforeUpdate,
    AfterUnpack,
    AfterInstall,
}

impl HookStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::BeforeUpdate => "before-update",
            Self::AfterUnpack => "after-unpack",
            Self::AfterInstall => "after-install",
        }
    }
}

pub struct HookContext<'a> {
    pub app_root: &'a Path,
    pub toolkit_root: &'a Path,
    pub downloads: &'a Path,
    pub staging: Option<&'a Path>,
    pub install: &'a Path,
    pub version: Option<&'a str>,
}

/// Hook execution entry point. The `python` field memoizes interpreter
/// resolution for the whole run (see `python::PythonCache`); keeping it on
/// the runner instead of a process-wide static isolates concurrent tests.
#[derive(Default)]
pub struct HookRunner {
    python: PythonCache,
}

impl HookRunner {
    pub fn run(
        &self,
        actions: &[HookAction],
        stage: HookStage,
        tool: &Tool,
        context: &HookContext<'_>,
    ) -> Result<()> {
        for (index, action) in actions.iter().enumerate() {
            tracing::debug!(
                tool = %tool.id,
                hook = stage.as_str(),
                action = action_name(action),
                index,
                "running hook action"
            );
            if let Err(error) = run_action(action, tool, context, &self.python) {
                return Err(UpdaterError::Hook {
                    tool: tool.id.clone(),
                    stage: stage.as_str().to_owned(),
                    message: format!(
                        "action {} at index {index} failed: {error:#}",
                        action_name(action)
                    ),
                }
                .into());
            }
        }
        Ok(())
    }
}

fn run_action(
    action: &HookAction,
    tool: &Tool,
    context: &HookContext<'_>,
    python_cache: &PythonCache,
) -> Result<()> {
    match action {
        HookAction::Rename { from, to } => {
            native::rename_one(require_staging(context, "rename")?, from, to)
        }
        HookAction::MoveContents { from, to } => {
            native::move_contents(require_staging(context, "move-contents")?, from, to)
        }
        HookAction::Python {
            script,
            args,
            timeout_seconds,
            working_directory,
            environment_mode,
            environment,
        } => python::run(
            python_cache,
            python::PythonHookSpec {
                script,
                args,
                timeout_seconds: *timeout_seconds,
                working_directory: *working_directory,
                environment_mode: *environment_mode,
                environment,
            },
            tool,
            context,
        ),
    }
}

fn action_name(action: &HookAction) -> &'static str {
    match action {
        HookAction::Rename { .. } => "rename",
        HookAction::MoveContents { .. } => "move-contents",
        HookAction::Python { .. } => "python",
    }
}

fn require_staging<'a>(context: &'a HookContext<'_>, action: &str) -> Result<&'a Path> {
    context
        .staging
        .ok_or_else(|| anyhow::anyhow!("{action} is only available during after-unpack"))
}
