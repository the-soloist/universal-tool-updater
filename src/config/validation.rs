mod hooks;
mod release;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use regex::Regex;
use reqwest::header::HeaderValue;

use crate::config::model::{InstallConfig, ManifestFile, ToolConfig};
use crate::domain::{ArtifactConfig, InputMode, InstallSpec, OutputMode, effective_output_mode};
use crate::error::UpdaterError;
use crate::paths::{is_portable_filename, is_portable_relative_path};

pub(super) fn validate_manifest_values(path: &Path, manifest: &ManifestFile) -> Result<()> {
    for (field, value) in [
        ("paths.toolkit_root", &manifest.paths.toolkit_root),
        ("paths.downloads", &manifest.paths.downloads),
        ("paths.state", &manifest.paths.state),
    ] {
        if value.as_os_str().is_empty() || value.to_string_lossy().trim().is_empty() {
            return Err(UpdaterError::config(path, format!("{field} must not be empty")).into());
        }
    }
    if manifest.paths.staging.as_ref().is_some_and(|value| {
        value.as_os_str().is_empty() || value.to_string_lossy().trim().is_empty()
    }) {
        return Err(UpdaterError::config(path, "paths.staging must not be empty").into());
    }

    let network = &manifest.network;
    if network.user_agent.trim().is_empty() {
        return Err(UpdaterError::config(path, "network.user_agent must not be empty").into());
    }
    if network.user_agent.parse::<HeaderValue>().is_err() {
        return Err(UpdaterError::config(
            path,
            "network.user_agent must be a valid HTTP header value",
        )
        .into());
    }
    if network.timeout_seconds == 0 {
        return Err(UpdaterError::config(
            path,
            "network.timeout_seconds must be greater than zero",
        )
        .into());
    }
    if network.jobs == 0 {
        return Err(UpdaterError::config(path, "network.jobs must be greater than zero").into());
    }
    if !is_portable_environment_name(&network.github_token_env) {
        return Err(UpdaterError::config(
            path,
            "network.github_token_env must be a portable environment variable name",
        )
        .into());
    }

    validate_archive_name(
        path,
        "defaults.install.archive_name",
        &manifest.defaults.install.archive_name,
        manifest.defaults.install.save,
    )
}

pub(super) fn validate_tool_config(
    path: &Path,
    id: &str,
    tool: &ToolConfig,
    app_root: &Path,
    allow_insecure_transports: bool,
) -> Result<()> {
    validate_id(path, id)?;
    if let Some(name) = &tool.name {
        if name.trim().is_empty() {
            return Err(
                UpdaterError::config(path, format!("tool {id}: name must not be empty")).into(),
            );
        }
        if name.trim() != name {
            return Err(UpdaterError::config(
                path,
                format!("tool {id}: name must not have leading or trailing whitespace"),
            )
            .into());
        }
        if name.chars().any(char::is_control) {
            return Err(UpdaterError::config(
                path,
                format!("tool {id}: name must not contain control characters"),
            )
            .into());
        }
    }
    release::validate(path, id, &tool.release, allow_insecure_transports)?;
    release::validate_artifacts(
        path,
        id,
        &tool.release,
        &tool.artifacts,
        allow_insecure_transports,
    )?;
    hooks::validate(path, id, &tool.hooks, app_root)?;
    validate_install_config(path, id, &tool.install)
}

pub(super) fn validate_install_spec(
    path: &Path,
    id: &str,
    name: &str,
    artifacts: &[ArtifactConfig],
    install: &InstallSpec,
) -> Result<()> {
    validate_archive_name(
        path,
        &format!("tool {id}: archive_name"),
        &install.archive_name,
        install.save,
    )?;

    let effective_output = effective_output_mode(install.input, install.save, artifacts);
    if effective_output == OutputMode::Archive {
        let rendered = install
            .archive_name
            .replace("{id}", id)
            .replace("{name}", name)
            .replace("{version}", "1.0.0");
        if !is_portable_filename(Path::new(&rendered)) {
            return Err(UpdaterError::config(
                path,
                format!(
                    "tool {id}: archive_name renders to a non-portable filename with this id and name"
                ),
            )
            .into());
        }
    }

    if install
        .archive_password
        .as_deref()
        .is_some_and(|password| password.is_empty())
    {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: archive_password must not be empty"),
        )
        .into());
    }
    if install.input == InputMode::Copy && install.archive_password.is_some() {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: archive_password conflicts with input copy"),
        )
        .into());
    }
    if effective_output == OutputMode::Archive && !install.symlinks.is_empty() {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: symlinks require directory output and conflict with save archive"),
        )
        .into());
    }

    let mut executables = BTreeSet::<&PathBuf>::new();
    if let Some(duplicate) = install
        .executable
        .iter()
        .find(|value| !executables.insert(*value))
    {
        return Err(UpdaterError::config(
            path,
            format!(
                "tool {id}: duplicate executable path {}",
                duplicate.display()
            ),
        )
        .into());
    }

    let mut targets = BTreeSet::<&PathBuf>::new();
    for link in &install.symlinks {
        if !targets.insert(&link.to) {
            return Err(UpdaterError::config(
                path,
                format!("tool {id}: duplicate symlink target {}", link.to.display()),
            )
            .into());
        }
        if install.destination.join(&link.from) == link.to {
            return Err(UpdaterError::config(
                path,
                format!(
                    "tool {id}: symlink target {} conflicts with its source",
                    link.to.display()
                ),
            )
            .into());
        }
    }
    Ok(())
}

fn validate_archive_name(path: &Path, field: &str, value: &str, save: OutputMode) -> Result<()> {
    if !is_portable_filename(Path::new(value)) {
        return Err(UpdaterError::config(
            path,
            format!("{field} must be a portable filename, not a path"),
        )
        .into());
    }
    validate_placeholders(value, &["id", "name", "version"])
        .map_err(|message| UpdaterError::config(path, format!("{field} {message}")))?;
    if save == OutputMode::Archive && !value.to_ascii_lowercase().ends_with(".7z") {
        return Err(
            UpdaterError::config(path, format!("{field} must use the .7z extension")).into(),
        );
    }
    Ok(())
}

pub(super) fn validate_placeholders(
    value: &str,
    allowed: &[&str],
) -> std::result::Result<(), String> {
    let mut remaining = value;
    loop {
        let Some(open) = remaining.find('{') else {
            if remaining.contains('}') {
                return Err("contains an unmatched '}'".to_owned());
            }
            return Ok(());
        };
        if remaining[..open].contains('}') {
            return Err("contains an unmatched '}'".to_owned());
        }
        let after_open = &remaining[open + 1..];
        let Some(close) = after_open.find('}') else {
            return Err("contains an unmatched '{'".to_owned());
        };
        let placeholder = &after_open[..close];
        if placeholder.contains('{') || !allowed.contains(&placeholder) {
            return Err(format!(
                "contains unsupported placeholder {{{placeholder}}}"
            ));
        }
        remaining = &after_open[close + 1..];
    }
}

fn is_portable_environment_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
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
