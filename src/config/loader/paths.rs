use std::fs;
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
    let left = canonicalized(left);
    let right = canonicalized(right);
    path_starts_with(&left, &right) || path_starts_with(&right, &left)
}

thread_local! {
    /// Memoizes canonicalized comparison keys so repeated overlap checks during
    /// one configuration load do not re-stat the same paths.
    static CANONICALIZED: std::cell::RefCell<std::collections::HashMap<PathBuf, PathBuf>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Canonicalizes the longest existing ancestor so differently-spelled but equal
/// directories (case, symlinks, verbatim prefixes) compare equal; paths whose
/// ancestors cannot be canonicalized stay in textual form.
fn canonicalized(path: &Path) -> PathBuf {
    let remembered = CANONICALIZED.with(|cache| cache.borrow().get(path).cloned());
    if let Some(remembered) = remembered {
        return remembered;
    }
    let result = canonicalize_longest_ancestor(path);
    CANONICALIZED.with(|cache| {
        cache
            .borrow_mut()
            .insert(path.to_path_buf(), result.clone())
    });
    result
}

fn canonicalize_longest_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while current.as_os_str() != "" {
        if let Ok(canonical) = fs::canonicalize(&current) {
            let rest = path.strip_prefix(&current).unwrap_or(Path::new(""));
            return comparable_form(canonical.join(rest));
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    path.to_path_buf()
}

/// Windows fs::canonicalize returns \\?\-prefixed verbatim paths that never
/// compare equal to plain spellings; strip the prefix for comparison only.
#[cfg(windows)]
fn comparable_form(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
fn comparable_form(path: PathBuf) -> PathBuf {
    path
}

fn path_starts_with(path: &Path, base: &Path) -> bool {
    // 统一按不区分大小写比较路径组件，使各平台的路径重叠检查遵循 Windows 语义。
    let mut path = path.components();
    base.components().all(|expected| {
        path.next().is_some_and(|actual| {
            // ASCII-only folding: Windows guarantees case-insensitivity for
            // ASCII but not for other scripts, so e.g. Cyrillic look-alikes
            // must not compare equal.
            actual
                .as_os_str()
                .eq_ignore_ascii_case(expected.as_os_str())
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::paths_overlap;

    #[test]
    fn detects_overlap_through_differently_cased_existing_ancestors() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("Real");
        std::fs::create_dir(&real).unwrap();

        let staging = real.join("staging");
        let state = directory.path().join("real/staging/state.yaml");
        assert!(paths_overlap(&staging, &state));
    }

    #[test]
    fn keeps_disjoint_directories_apart() {
        let directory = tempdir().unwrap();
        let staging = directory.path().join("downloads/staging");
        let state = directory.path().join("toolkit/.updater/state.yaml");
        assert!(!paths_overlap(&staging, &state));
    }

    #[test]
    fn falls_back_to_case_insensitive_text_comparison_for_missing_paths() {
        let staging = Path::new("/definitely/missing/staging");
        let state = Path::new("/Definitely/Missing/Staging/state.yaml");
        assert!(paths_overlap(staging, state));
        let elsewhere = Path::new("/definitely/missing-elsewhere/state.yaml");
        assert!(!paths_overlap(staging, elsewhere));
    }

    #[test]
    fn does_not_fold_non_ascii_case_differences() {
        let directory = tempdir().unwrap();
        let staging = directory.path().join("стагинг");
        let state = directory.path().join("СТАГИНГ/state.yaml");
        assert!(
            !paths_overlap(&staging, &state),
            "Windows only guarantees ASCII case-insensitivity"
        );
    }

    #[cfg(windows)]
    #[test]
    fn matches_verbatim_and_plain_spellings_of_the_same_directory() {
        let directory = tempdir().unwrap();
        let staging =
            std::path::PathBuf::from(format!(r"\\?\{}\staging", directory.path().display()));
        let state = directory.path().join("staging/state.yaml");
        assert!(paths_overlap(&staging, &state));
    }
}
