use std::path::{Component, Path, PathBuf};

use crate::error::UpdaterError;

pub fn expand_path(raw: &Path) -> Result<PathBuf, UpdaterError> {
    let text = raw.to_string_lossy();
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| UpdaterError::config(raw, "cannot determine the user home directory"))?;
        if text.len() == 1 {
            return Ok(PathBuf::from(home));
        }
        return Ok(PathBuf::from(home).join(&text[2..]));
    }
    Ok(raw.to_path_buf())
}

pub fn resolve_from(base: &Path, raw: &Path) -> Result<PathBuf, UpdaterError> {
    let expanded = expand_path(raw)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(base.join(expanded))
    }
}

pub fn safe_filename(value: &str) -> Option<String> {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .map(ToOwned::to_owned)
}

pub fn is_portable_filename(path: &Path) -> bool {
    path.to_str().is_some_and(is_portable_filename_pattern)
        && path.file_name() == Some(path.as_os_str())
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub fn is_portable_filename_pattern(value: &str) -> bool {
    !value.is_empty() && !value.contains(['/', '\\', ':'])
}

pub fn is_portable_relative_path(path: &Path, allow_current: bool) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.to_string_lossy().contains(['\\', ':'])
        && path.components().all(|part| {
            matches!(part, Component::Normal(_))
                || (allow_current && matches!(part, Component::CurDir))
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        is_portable_filename, is_portable_filename_pattern, is_portable_relative_path,
        safe_filename,
    };

    #[test]
    fn removes_parent_components_from_filenames() {
        assert_eq!(safe_filename("../../tool.zip").as_deref(), Some("tool.zip"));
        assert_eq!(safe_filename(".."), None);
    }

    #[test]
    fn validates_portable_paths_consistently() {
        assert!(is_portable_filename(Path::new("tool.exe")));
        assert!(is_portable_filename_pattern("tool-*.exe"));
        assert!(is_portable_relative_path(Path::new("bin/tool"), false));
        assert!(is_portable_relative_path(Path::new("."), true));
        assert!(!is_portable_relative_path(Path::new("."), false));
        assert!(!is_portable_relative_path(Path::new("../tool"), true));
        assert!(!is_portable_filename(Path::new("dir/tool.exe")));
        assert!(!is_portable_filename_pattern("dir\\tool-*.exe"));
    }
}
