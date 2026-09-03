use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Paths;
use crate::config::model::{DefaultsConfig, InstallConfig, ToolConfig};
use crate::config::validation::validate_install_spec;
use crate::domain::{InputMode, InstallSpec, SymlinkSpec, Tool};
use crate::error::UpdaterError;
use crate::paths::installation_backup_path;

use super::paths::{paths_overlap, resolve_destination};

pub(super) fn materialize(
    path: &Path,
    id: String,
    profile: String,
    raw: ToolConfig,
    defaults: &DefaultsConfig,
    paths: &Paths,
) -> Result<Tool> {
    let install = resolve_install(path, &id, &raw.install, defaults, paths)?;
    let name = raw.name.unwrap_or_else(|| id.clone());
    validate_install_spec(path, &id, &name, &raw.artifacts, &install)?;
    let tool = Tool {
        name,
        id,
        profile,
        enabled: raw.enabled,
        release: raw.release,
        artifacts: raw.artifacts,
        install,
        hooks: raw.hooks,
    };
    validate_runtime_path_conflicts(path, &tool, paths)?;
    Ok(tool)
}

fn resolve_install(
    path: &Path,
    id: &str,
    raw: &InstallConfig,
    defaults: &DefaultsConfig,
    paths: &Paths,
) -> Result<InstallSpec> {
    let input = raw.input.unwrap_or(defaults.install.input);
    let destination = resolve_destination(&paths.toolkit_root, &raw.destination)
        .map_err(|message| UpdaterError::config(path, format!("tool {id}: {message}")))?;
    let destination = destination_for_input(destination, input)
        .map_err(|message| UpdaterError::config(path, format!("tool {id}: {message}")))?;
    let symlinks = raw
        .symlinks
        .iter()
        .map(|link| {
            Ok(SymlinkSpec {
                from: link.from.clone(),
                to: resolve_destination(&paths.toolkit_root, &link.to).map_err(|message| {
                    UpdaterError::config(path, format!("tool {id}: symlink: {message}"))
                })?,
            })
        })
        .collect::<Result<Vec<_>, UpdaterError>>()?;
    Ok(InstallSpec {
        destination,
        input,
        existing: raw.existing.unwrap_or(defaults.install.existing),
        save: raw.save.unwrap_or(defaults.install.save),
        strip_single_root: raw
            .strip_single_root
            .unwrap_or(defaults.install.strip_single_root),
        create_destination: raw
            .create_destination
            .unwrap_or(defaults.create_destination),
        archive_name: raw
            .archive_name
            .clone()
            .unwrap_or_else(|| defaults.install.archive_name.clone()),
        archive_password: raw.archive_password.clone(),
        allow_symlinks_in_archive: raw.allow_symlinks_in_archive,
        executable: raw.executable.clone(),
        symlinks,
    })
}

fn destination_for_input(
    mut destination: PathBuf,
    input: InputMode,
) -> std::result::Result<PathBuf, String> {
    if input != InputMode::Copy {
        return Ok(destination);
    }
    if destination
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("release"))
    {
        return Err(
            "destination must not end with 'release' when input is copy; the updater appends it automatically"
                .to_owned(),
        );
    }
    destination.push("release");
    Ok(destination)
}

fn validate_runtime_path_conflicts(path: &Path, tool: &Tool, paths: &Paths) -> Result<()> {
    let destination = &tool.install.destination;
    if destination.file_name().is_none() {
        return Err(UpdaterError::config(
            path,
            format!(
                "tool {}: destination must identify a concrete file or directory",
                tool.id
            ),
        )
        .into());
    }
    if destination == &paths.toolkit_root {
        return Err(UpdaterError::config(
            path,
            format!(
                "tool {}: destination must not equal paths.toolkit_root",
                tool.id
            ),
        )
        .into());
    }
    validate_reserved_path(path, tool, "destination", destination, paths)?;
    if let Some(backup) = installation_backup_path(destination) {
        validate_reserved_path(path, tool, "transaction backup", &backup, paths)?;
    }
    for link in &tool.install.symlinks {
        validate_reserved_path(path, tool, "symlink target", &link.to, paths)?;
    }
    if let Some(marker) = tool.version_marker_path() {
        validate_reserved_path(path, tool, "version marker", &marker, paths)?;
        if let Some(backup) = installation_backup_path(&marker) {
            validate_reserved_path(path, tool, "transaction backup", &backup, paths)?;
        }
    }
    Ok(())
}

fn validate_reserved_path(
    path: &Path,
    tool: &Tool,
    kind: &str,
    candidate: &Path,
    paths: &Paths,
) -> Result<()> {
    for (field, reserved) in reserved_paths(paths) {
        if paths_overlap(candidate, reserved) {
            return Err(UpdaterError::config(
                path,
                format!(
                    "tool {}: {kind} {} conflicts with {field} {}",
                    tool.id,
                    candidate.display(),
                    reserved.display()
                ),
            )
            .into());
        }
    }
    Ok(())
}

fn reserved_paths(paths: &Paths) -> [(&'static str, &Path); 3] {
    [
        ("paths.downloads", &paths.downloads),
        ("paths.staging", &paths.staging),
        ("paths.state", &paths.state),
    ]
}
