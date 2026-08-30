use std::path::{Component, Path, PathBuf};

use url::Url;

use crate::error::UpdaterError;

pub fn expand_path(raw: &Path) -> Result<PathBuf, UpdaterError> {
    let text = raw.to_string_lossy();
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        let home = select_home(std::env::var_os("USERPROFILE"), std::env::var_os("HOME"))
            .ok_or_else(|| UpdaterError::config(raw, "cannot determine the user home directory"))?;
        if text.len() == 1 {
            return Ok(PathBuf::from(home));
        }
        return Ok(PathBuf::from(home).join(&text[2..]));
    }
    Ok(raw.to_path_buf())
}

/// Windows resolves the user home from USERPROFILE first (HOME is often a
/// Git-Bash override pointing elsewhere); Unix keeps HOME-only semantics.
#[cfg(windows)]
fn select_home(
    userprofile: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    userprofile.or(home)
}

#[cfg(not(windows))]
fn select_home(
    userprofile: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    home
}

pub fn resolve_from(base: &Path, raw: &Path) -> Result<PathBuf, UpdaterError> {
    let expanded = expand_path(raw)?;
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    Ok(normalize_path(&resolved))
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized
                    .components()
                    .next_back()
                    .is_some_and(|component| matches!(component, Component::Normal(_)))
                {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
        }
    }
    normalized
}

pub(crate) fn installation_backup_path(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".utu-backup");
    Some(path.with_file_name(name))
}

pub fn safe_filename(value: &str) -> Option<String> {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_portable_filename(Path::new(name)))
        .map(ToOwned::to_owned)
}

pub(crate) fn filename_from_url(value: &str) -> Option<String> {
    Url::parse(value).ok().and_then(|url| {
        url.path_segments()
            .and_then(|mut parts| parts.next_back())
            .and_then(|name| {
                decode_url_component(name)
                    .as_deref()
                    .and_then(safe_filename)
                    .or_else(|| safe_filename(name))
            })
    })
}

pub(crate) fn decode_url_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(bytes.get(index + 1).copied()?)?;
            let low = hex(bytes.get(index + 2).copied()?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub fn is_portable_filename(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|value| is_portable_component(value, false))
        && path.file_name() == Some(path.as_os_str())
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub fn is_portable_filename_pattern(value: &str) -> bool {
    is_portable_component(value, true)
}

pub fn is_portable_relative_path(path: &Path, allow_current: bool) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|part| match part {
            Component::Normal(value) => value
                .to_str()
                .is_some_and(|value| is_portable_component(value, false)),
            Component::CurDir => allow_current,
            _ => false,
        })
}

pub(crate) fn is_portable_component(value: &str, allow_wildcards: bool) -> bool {
    if value.is_empty()
        || value.ends_with([' ', '.'])
        || value.chars().any(|character| {
            character.is_control()
                || matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|')
                || (!allow_wildcards && matches!(character, '*' | '?'))
        })
    {
        return false;
    }
    if allow_wildcards && value.contains(['*', '?']) {
        return true;
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !matches!(
            stem.as_bytes(),
            [b'C', b'O', b'M', b'1'..=b'9'] | [b'L', b'P', b'T', b'1'..=b'9']
        )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        filename_from_url, installation_backup_path, is_portable_filename,
        is_portable_filename_pattern, is_portable_relative_path, normalize_path, safe_filename,
        select_home,
    };

    #[test]
    fn resolves_the_windows_home_from_userprofile_first() {
        #[cfg(windows)]
        {
            let userprofile = Some(std::ffi::OsString::from(r"C:\Users\demo"));
            let home = Some(std::ffi::OsString::from(
                r"C:\Users\demo\AppData\Local\Programs\Git\home",
            ));
            assert_eq!(select_home(userprofile.clone(), home.clone()), userprofile);
            assert_eq!(
                select_home(None, home.clone()),
                Some(std::ffi::OsString::from(
                    r"C:\Users\demo\AppData\Local\Programs\Git\home"
                ))
            );
            assert!(select_home(userprofile, None).is_some());
        }
        #[cfg(not(windows))]
        {
            let userprofile = Some(std::ffi::OsString::from("/home/demo"));
            let home = Some(std::ffi::OsString::from("/real/home"));
            assert_eq!(select_home(userprofile, home.clone()), home);
            assert_eq!(select_home(None, None), None);
        }
    }

    #[test]
    fn removes_parent_components_from_filenames() {
        assert_eq!(safe_filename("../../tool.zip").as_deref(), Some("tool.zip"));
        assert_eq!(safe_filename(".."), None);
        assert_eq!(safe_filename("CON.txt"), None);
        assert_eq!(safe_filename("invalid:name.zip"), None);
        assert_eq!(
            filename_from_url("https://example.com/releases/tool.zip?download=1").as_deref(),
            Some("tool.zip")
        );
        assert_eq!(
            filename_from_url("https://example.com/releases/%E5%B7%A5%E5%85%B7.zip").as_deref(),
            Some("工具.zip")
        );
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
        assert!(!is_portable_filename(Path::new("tool?.exe")));
        assert!(!is_portable_filename(Path::new("CON.txt")));
        assert!(!is_portable_relative_path(Path::new("bin/NUL"), false));
        assert!(!is_portable_relative_path(
            Path::new("bin/trailing."),
            false
        ));
        assert_eq!(
            normalize_path(Path::new("/Toolkit/.updater/../Demo/.version")),
            Path::new("/Toolkit/Demo/.version")
        );
        assert_eq!(
            installation_backup_path(Path::new("/Toolkit/Demo")),
            Some(Path::new("/Toolkit/Demo.utu-backup").to_path_buf())
        );
        assert_eq!(installation_backup_path(Path::new("/")), None);
    }
}
