mod hooks;
mod release;

use std::path::Path;

use anyhow::Result;
use regex::Regex;

use crate::config::model::{InstallConfig, ToolConfig};
use crate::domain::{InstallSpec, OutputMode};
use crate::error::UpdaterError;
use crate::paths::{is_portable_filename, is_portable_relative_path};

pub(super) fn validate_tool_config(
    path: &Path,
    id: &str,
    tool: &ToolConfig,
    app_root: &Path,
) -> Result<()> {
    validate_id(path, id)?;
    release::validate(path, id, &tool.release)?;
    release::validate_artifacts(path, id, &tool.release, &tool.artifacts)?;
    hooks::validate(path, id, &tool.hooks, app_root)?;
    validate_install_config(path, id, &tool.install)
}

pub(super) fn validate_install_spec(path: &Path, id: &str, install: &InstallSpec) -> Result<()> {
    if !is_portable_filename(Path::new(&install.archive_name)) {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: archive_name must be a portable filename, not a path"),
        )
        .into());
    }
    if install.save == OutputMode::Archive
        && !install.archive_name.to_ascii_lowercase().ends_with(".7z")
    {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: archive_name must use the .7z extension"),
        )
        .into());
    }
    Ok(())
}

fn validate_install_config(path: &Path, id: &str, install: &InstallConfig) -> Result<()> {
    for executable in &install.executable {
        validate_relative_tool_path(path, id, "executable", executable)?;
    }
    for link in &install.symlinks {
        validate_relative_tool_path(path, id, "symlink source", &link.from)?;
    }
    Ok(())
}

fn validate_relative_tool_path(path: &Path, id: &str, field: &str, value: &Path) -> Result<()> {
    if !is_portable_relative_path(value, false) {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: {field} must be a portable, safe relative path"),
        )
        .into());
    }
    Ok(())
}

fn validate_id(path: &Path, id: &str) -> Result<()> {
    let valid = Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").expect("static regex");
    if !valid.is_match(id) {
        return Err(UpdaterError::config(
            path,
            format!(
                "invalid tool id {id:?}; use lowercase kebab-case such as 'context-menu-manager'"
            ),
        )
        .into());
    }
    Ok(())
}
