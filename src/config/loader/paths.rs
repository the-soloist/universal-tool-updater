use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Paths;
use crate::config::model::ManifestFile;
use crate::error::UpdaterError;
use crate::paths::{expand_path, is_portable_relative_path, normalize_path, resolve_from};

pub(super) fn resolve_manifest_path(app_root: &Path, manifest_path: &Path) -> PathBuf {
    if manifest_path.is_absolute() {
        manifest_path.to_path_buf()
    } else {
        app_root.join(manifest_path)
    }
}

pub(super) fn resolve_paths(
    app_root: &Path,
    manifest_path: &Path,
    manifest: &ManifestFile,
) -> Result<Paths> {
    let toolkit_root = resolve_from(app_root, &manifest.paths.toolkit_root)?;
    let updater_root = updater_directory()?;
    let downloads = resolve_setting_path(&updater_root, &manifest.paths.downloads)?;
    let staging = manifest
        .paths
        .staging
        .as_deref()
        .map(|path| resolve_setting_path(&updater_root, path))
        .transpose()?
        .unwrap_or_else(|| downloads.join("staging"));
    let state = resolve_setting_path(&toolkit_root, &manifest.paths.state)?;
    if paths_overlap(&staging, &state) {
        return Err(UpdaterError::config(
            manifest_path,
            format!(
                "paths.staging {} conflicts with paths.state {}",
                staging.display(),
                state.display()
            ),
        )
        .into());
    }
    Ok(Paths {
        downloads,
        staging,
        state,
        toolkit_root,
    })
}

pub(super) fn resolve_include(manifest: &Path, directory: &Path, include: &str) -> Result<PathBuf> {
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

pub(super) fn profile_name(path: &Path) -> Result<String> {
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

pub(super) fn resolve_destination(root: &Path, raw: &Path) -> std::result::Result<PathBuf, String> {
    if raw.as_os_str().is_empty() {
        return Err("destination must not be empty".to_owned());
    }
    let expanded = expand_path(raw).map_err(|error| error.to_string())?;
    if expanded
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!(
            "relative destination {} may not contain '..'",
            raw.display()
        ));
    }
    if expanded.is_absolute() {
        return Ok(expanded);
    }
    if !is_portable_relative_path(&expanded, false) {
        return Err(format!(
            "relative destination {} must be a portable, safe relative path",
            raw.display()
        ));
    }
    Ok(root.join(expanded))
}

pub(super) fn paths_overlap(left: &Path, right: &Path) -> bool {
    path_starts_with(left, right) || path_starts_with(right, left)
}

fn path_starts_with(path: &Path, base: &Path) -> bool {
    let mut path = path.components();
    base.components().all(|expected| {
        path.next().is_some_and(|actual| {
            actual.as_os_str().to_string_lossy().to_lowercase()
                == expected.as_os_str().to_string_lossy().to_lowercase()
        })
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

fn resolve_setting_path(base: &Path, raw: &Path) -> Result<PathBuf> {
    let expanded = expand_path(raw)?;
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    Ok(normalize_path(&resolved))
}
