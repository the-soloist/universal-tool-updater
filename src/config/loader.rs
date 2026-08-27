use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::model::{
    DefaultsConfig, InstallConfig, ManifestFile, SCHEMA_VERSION, ToolConfig, ToolFile,
};
use crate::config::validation::{validate_install_spec, validate_tool_config};
use crate::config::{AppConfig, Paths};
use crate::domain::{InputMode, InstallSpec, SymlinkSpec, Tool};
use crate::error::UpdaterError;
use crate::paths::{expand_path, is_portable_relative_path, resolve_from};

pub fn load(manifest_path: &Path) -> Result<AppConfig> {
    let app_root = std::env::current_dir().context("cannot determine the working directory")?;
    let manifest_path = resolve_manifest_path(&app_root, manifest_path);
    let manifest: ManifestFile = read_yaml(&manifest_path)?;
    validate_manifest(&manifest_path, &manifest)?;
    let paths = resolve_paths(&app_root, &manifest)?;

    let manifest_dir = manifest_path.parent().unwrap_or(Path::new("."));
    let mut tools = BTreeMap::new();
    let mut profiles = BTreeMap::<String, PathBuf>::new();
    let mut destinations = BTreeMap::<PathBuf, String>::new();
    let mut symlink_targets = BTreeMap::<PathBuf, String>::new();
    for include in &manifest.include {
        let path = resolve_include(&manifest_path, manifest_dir, include)?;
        let profile = profile_name(&path)?;
        if let Some(previous) = profiles.insert(profile.clone(), path.clone()) {
            return Err(UpdaterError::config(
                &manifest_path,
                format!(
                    "include files {} and {} define the same profile {profile}",
                    previous.display(),
                    path.display()
                ),
            )
            .into());
        }
        let tool_file: ToolFile = read_yaml(&path)?;
        for (id, raw) in tool_file.tools {
            validate_tool_config(&path, &id, &raw, &app_root)?;
            if tools.contains_key(&id) {
                return Err(UpdaterError::config(&path, format!("duplicate tool id {id}")).into());
            }
            let tool = materialize_tool(
                &path,
                id.clone(),
                profile.clone(),
                raw,
                &manifest.defaults,
                &paths,
            )?;
            if let Some((destination, previous)) = destinations.iter().find(|(destination, _)| {
                tool.install.destination.starts_with(destination)
                    || destination.starts_with(&tool.install.destination)
            }) {
                return Err(UpdaterError::config(
                    &path,
                    format!(
                        "tools {previous} and {id} have overlapping destinations {} and {}",
                        destination.display(),
                        tool.install.destination.display(),
                    ),
                )
                .into());
            }
            destinations.insert(tool.install.destination.clone(), id.clone());
            for link in &tool.install.symlinks {
                if let Some(previous) = symlink_targets.insert(link.to.clone(), id.clone()) {
                    return Err(UpdaterError::config(
                        &path,
                        format!(
                            "tools {previous} and {id} share symlink target {}",
                            link.to.display()
                        ),
                    )
                    .into());
                }
            }
            tools.insert(id, tool);
        }
    }

    for (target, owner) in &symlink_targets {
        if let Some((destination, destination_owner)) = destinations
            .iter()
            .find(|(_, destination_owner)| *destination_owner != owner)
            .filter(|(destination, _)| target.starts_with(destination))
        {
            return Err(UpdaterError::config(
                &manifest_path,
                format!(
                    "tool {owner} symlink target {} overlaps tool {destination_owner} destination {}",
                    target.display(),
                    destination.display()
                ),
            )
            .into());
        }
    }

    Ok(AppConfig {
        app_root,
        paths,
        network: manifest.network,
        tools,
    })
}

fn resolve_manifest_path(app_root: &Path, manifest_path: &Path) -> PathBuf {
    if manifest_path.is_absolute() {
        manifest_path.to_path_buf()
    } else {
        app_root.join(manifest_path)
    }
}

fn validate_manifest(path: &Path, manifest: &ManifestFile) -> Result<()> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(UpdaterError::config(
            path,
            format!(
                "unsupported schema version {}; expected {SCHEMA_VERSION}",
                manifest.schema_version
            ),
        )
        .into());
    }
    if manifest.include.is_empty() {
        return Err(UpdaterError::config(path, "include must not be empty").into());
    }
    if manifest.network.jobs == 0 {
        return Err(UpdaterError::config(path, "network.jobs must be greater than zero").into());
    }
    Ok(())
}

fn resolve_paths(app_root: &Path, manifest: &ManifestFile) -> Result<Paths> {
    let toolkit_root = resolve_from(app_root, &manifest.paths.toolkit_root)?;
    let updater_root = updater_directory()?;
    Ok(Paths {
        downloads: resolve_setting_path(&updater_root, &manifest.paths.downloads)?,
        state: resolve_setting_path(&toolkit_root, &manifest.paths.state)?,
        toolkit_root,
    })
}

fn updater_directory() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("cannot determine the updater path")?;
    executable.parent().map(Path::to_path_buf).ok_or_else(|| {
        anyhow::anyhow!(
            "updater path {} has no parent directory",
            executable.display()
        )
    })
}

fn resolve_include(manifest: &Path, directory: &Path, include: &str) -> Result<PathBuf> {
    let relative = Path::new(include);
    if !is_portable_relative_path(relative, true) {
        return Err(UpdaterError::config(
            manifest,
            format!("include path {include:?} must stay inside the manifest directory"),
        )
        .into());
    }
    Ok(directory.join(relative))
}

fn profile_name(path: &Path) -> Result<String> {
    if path.extension().and_then(|value| value.to_str()) != Some("yaml") {
        return Err(
            UpdaterError::config(path, "profile include must use the .yaml extension").into(),
        );
    }
    if path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("manifest.yaml"))
    {
        return Err(
            UpdaterError::config(path, "manifest.yaml cannot be included as a profile").into(),
        );
    }
    path.file_stem()
        .and_then(|value| value.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| UpdaterError::config(path, "invalid profile filename").into())
}

fn materialize_tool(
    path: &Path,
    id: String,
    profile: String,
    raw: ToolConfig,
    defaults: &DefaultsConfig,
    paths: &Paths,
) -> Result<Tool> {
    let install = resolve_install(path, &id, &raw.install, defaults, paths)?;
    validate_install_spec(path, &id, &install)?;
    Ok(Tool {
        name: raw.name.unwrap_or_else(|| id.clone()),
        id,
        profile,
        enabled: raw.enabled,
        release: raw.release,
        artifacts: raw.artifacts,
        install,
        hooks: raw.hooks,
    })
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

fn read_yaml<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let input = fs::read_to_string(path)
        .with_context(|| format!("cannot read configuration {}", path.display()))?;
    yaml_serde::from_str(&input)
        .map_err(|error| UpdaterError::config(path, format!("invalid YAML: {error}")).into())
}

fn resolve_setting_path(base: &Path, raw: &Path) -> Result<PathBuf> {
    let expanded = expand_path(raw)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(base.join(expanded))
    }
}

fn resolve_destination(root: &Path, raw: &Path) -> std::result::Result<PathBuf, String> {
    if raw.as_os_str().is_empty() {
        return Err("destination must not be empty".to_owned());
    }
    let expanded = expand_path(raw).map_err(|error| error.to_string())?;
    if expanded.is_absolute() {
        return Ok(expanded);
    }
    if expanded
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!(
            "relative destination {} may not contain '..'",
            raw.display()
        ));
    }
    if !is_portable_relative_path(&expanded, false) {
        return Err(format!(
            "relative destination {} must be a portable, safe relative path",
            raw.display()
        ));
    }
    Ok(root.join(expanded))
}
