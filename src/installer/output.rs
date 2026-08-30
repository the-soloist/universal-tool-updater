use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use regex::Regex;

use crate::domain::{OutputMode, Tool, effective_output_mode as resolve_output_mode};
use crate::error::UpdaterError;
use crate::state::ArchiveState;

pub(super) fn effective_mode(tool: &Tool) -> OutputMode {
    resolve_output_mode(tool.install.input, tool.install.save, &tool.artifacts)
}

pub(crate) fn installation_matches(
    tool: &Tool,
    version: &str,
    archive: Option<&ArchiveState>,
) -> bool {
    if !tool.install.destination.is_dir() {
        return false;
    }
    if effective_mode(tool) == OutputMode::Archive {
        // 归档没有独立版本标记，必须确认已记录的文件身份仍与磁盘内容一致。
        let path = installed_archive_path(tool, version)
            .expect("archive output always has an installed archive path");
        return archive.is_some_and(|archive| archive.matches(&path));
    }

    let marker = tool
        .version_marker_path()
        .expect("directory output always has a version marker");
    fs::read_to_string(marker)
        .is_ok_and(|recorded| recorded.trim_end_matches(['\r', '\n']) == version)
}

pub(crate) fn installed_archive_state(tool: &Tool, version: &str) -> Result<Option<ArchiveState>> {
    installed_archive_path(tool, version)
        .map(|path| ArchiveState::capture(&path))
        .transpose()
}

pub(crate) fn installed_archive_path(tool: &Tool, version: &str) -> Option<PathBuf> {
    (effective_mode(tool) == OutputMode::Archive).then(|| {
        tool.install.destination.join(render_archive_name(
            &tool.install.archive_name,
            tool,
            version,
        ))
    })
}

pub(super) fn render_archive_name(template: &str, tool: &Tool, version: &str) -> String {
    template
        .replace("{id}", &tool.id)
        .replace("{name}", &tool.name)
        .replace("{version}", version)
}

pub(super) fn managed_archive_path(tool: &Tool, destination: &Path) -> Result<Option<PathBuf>> {
    let pattern = managed_archive_pattern(tool);
    let mut archives = Vec::new();
    for entry in fs::read_dir(destination)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| pattern.is_match(name)) {
            archives.push(entry.path());
        }
    }
    archives.sort();
    match archives.len() {
        0 => Ok(None),
        1 => Ok(archives.pop()),
        _ => Err(UpdaterError::Installation {
            tool: tool.id.clone(),
            message: format!(
                "destination {} contains multiple updater-managed archives",
                destination.display()
            ),
        }
        .into()),
    }
}

pub(super) fn managed_archive_pattern(tool: &Tool) -> Regex {
    let placeholders = ["{id}", "{name}", "{version}"];
    let mut remaining = tool.install.archive_name.as_str();
    let mut pattern = String::from("^");
    while let Some((index, placeholder)) = placeholders
        .iter()
        .filter_map(|placeholder| {
            remaining
                .find(placeholder)
                .map(|index| (index, *placeholder))
        })
        .min_by_key(|(index, _)| *index)
    {
        pattern.push_str(&regex::escape(&remaining[..index]));
        match placeholder {
            "{id}" => pattern.push_str(&regex::escape(&tool.id)),
            "{name}" => pattern.push_str(&regex::escape(&tool.name)),
            "{version}" => pattern.push_str(".+"),
            _ => unreachable!("placeholder comes from the static list"),
        }
        remaining = &remaining[index + placeholder.len()..];
    }
    pattern.push_str(&regex::escape(remaining));
    pattern.push('$');
    Regex::new(&pattern).expect("archive template is converted to a valid regular expression")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::domain::{InputMode, OutputMode};
    use crate::test_support::tool as test_tool;

    use crate::state::ArchiveState;

    use super::installation_matches;

    #[test]
    fn verifies_the_output_that_represents_the_recorded_version() {
        let directory = tempdir().unwrap();
        let mut extracted = test_tool("extracted", directory.path().join("extracted"));
        fs::create_dir(&extracted.install.destination).unwrap();
        assert!(!installation_matches(&extracted, "v1", None));
        fs::write(extracted.install.destination.join(".version"), "v1\n").unwrap();
        assert!(installation_matches(&extracted, "v1", None));
        assert!(!installation_matches(&extracted, "v2", None));

        let copy_root = directory.path().join("copied");
        let mut copied = test_tool("copied", copy_root.join("release"));
        copied.install.input = InputMode::Copy;
        fs::create_dir_all(&copied.install.destination).unwrap();
        fs::write(copy_root.join(".version"), "v1\n").unwrap();
        assert!(installation_matches(&copied, "v1", None));

        extracted.install.save = OutputMode::Archive;
        extracted.install.archive_name = "{id}#{version}.7z".to_owned();
        fs::remove_file(extracted.install.destination.join(".version")).unwrap();
        assert!(!installation_matches(&extracted, "v1", None));
        let archive = extracted.install.destination.join("extracted#v1.7z");
        fs::write(&archive, "archive").unwrap();
        let state = ArchiveState::capture(&archive).unwrap();
        assert!(installation_matches(&extracted, "v1", Some(&state)));
        assert!(!installation_matches(&extracted, "v2", Some(&state)));
        fs::write(&archive, "changed archive").unwrap();
        assert!(!installation_matches(&extracted, "v1", Some(&state)));
    }
}
