use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::config::model::{HookAction, HookConfig, HookWorkingDirectory};
use crate::error::UpdaterError;
use crate::paths::{is_portable_filename, is_portable_filename_pattern, is_portable_relative_path};

pub(super) fn validate(path: &Path, id: &str, hooks: &HookConfig, app_root: &Path) -> Result<()> {
    for (stage, actions) in [
        ("before_update", &hooks.before_update),
        ("after_unpack", &hooks.after_unpack),
        ("after_install", &hooks.after_install),
    ] {
        for (index, action) in actions.iter().enumerate() {
            validate_action(path, id, stage, index, action, app_root)?;
        }
    }
    Ok(())
}

fn validate_action(
    path: &Path,
    id: &str,
    stage: &str,
    index: usize,
    action: &HookAction,
    app_root: &Path,
) -> Result<()> {
    let action_error = |message: &str| {
        UpdaterError::config(
            path,
            format!("tool {id}: hook {stage} action {index}: {message}"),
        )
    };
    match action {
        HookAction::Rename { from, to } => {
            require_after_unpack(stage, &action_error, "rename")?;
            if !is_portable_filename_pattern(from) {
                return Err(action_error(
                    "rename source must be a non-empty portable filename pattern",
                )
                .into());
            }
            if !is_portable_filename(to) {
                return Err(action_error("rename destination must be a portable filename").into());
            }
        }
        HookAction::MoveContents { from, to } => {
            require_after_unpack(stage, &action_error, "move-contents")?;
            validate_action_path(from, false)
                .map_err(|message| action_error(&format!("source {message}")))?;
            validate_action_path(to, true)
                .map_err(|message| action_error(&format!("destination {message}")))?;
            if to.starts_with(from) {
                return Err(
                    action_error("move-contents destination may not be inside its source").into(),
                );
            }
        }
        HookAction::Python {
            script,
            timeout_seconds,
            working_directory,
            environment,
            ..
        } => {
            validate_action_path(script, false)
                .map_err(|message| action_error(&format!("script {message}")))?;
            if script
                .extension()
                .and_then(|extension| extension.to_str())
                .is_none_or(|extension| !extension.eq_ignore_ascii_case("py"))
            {
                return Err(action_error("Python script must use the .py extension").into());
            }
            if *working_directory == HookWorkingDirectory::Staging && stage != "after_unpack" {
                return Err(
                    action_error("staging working directory is only valid after_unpack").into(),
                );
            }
            if *timeout_seconds == 0 {
                return Err(action_error("timeout_seconds must be greater than zero").into());
            }
            validate_environment(&action_error, environment)?;
            let resolved = app_root.join(script);
            if !resolved.is_file() {
                return Err(action_error(&format!(
                    "Python script {} does not exist",
                    resolved.display()
                ))
                .into());
            }
        }
    }
    Ok(())
}

fn require_after_unpack(
    stage: &str,
    error: &impl Fn(&str) -> UpdaterError,
    action: &str,
) -> Result<()> {
    if stage != "after_unpack" {
        return Err(error(&format!("{action} is only valid after_unpack")).into());
    }
    Ok(())
}

fn validate_environment(
    error: &impl Fn(&str) -> UpdaterError,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    if let Some(name) = environment
        .keys()
        .find(|name| name.to_ascii_uppercase().starts_with("UTU_"))
    {
        return Err(error(&format!(
            "environment may not override reserved variable {name}"
        ))
        .into());
    }
    if let Some(name) = environment
        .keys()
        .find(|name| name.is_empty() || name.contains('='))
    {
        return Err(error(&format!("invalid environment variable name {name:?}")).into());
    }
    Ok(())
}

fn validate_action_path(path: &Path, allow_current: bool) -> std::result::Result<(), &'static str> {
    if !is_portable_relative_path(path, allow_current) {
        return Err("must be a portable, safe relative path");
    }
    Ok(())
}
