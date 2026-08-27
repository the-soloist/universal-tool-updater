use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;
use walkdir::WalkDir;

use crate::paths::{is_portable_filename, is_portable_filename_pattern, is_portable_relative_path};

pub(super) fn rename_one(root: &Path, from: &str, to: &Path) -> Result<()> {
    if !is_portable_filename(to) {
        bail!("rename destination must be a filename");
    }
    let matcher = wildcard_regex(from)?;
    let mut matches = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| matcher.is_match(name))
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    matches.sort();
    if matches.len() != 1 {
        bail!(
            "pattern {from:?} must match exactly one file under {}; found {}",
            root.display(),
            matches.len()
        );
    }
    let source = &matches[0];
    let destination = source.parent().unwrap_or(root).join(to);
    if source == &destination {
        return Ok(());
    }
    if fs::symlink_metadata(&destination).is_ok() {
        bail!(
            "rename destination {} already exists",
            destination.display()
        );
    }
    fs::rename(source, &destination).with_context(|| {
        format!(
            "cannot rename {} to {}",
            source.display(),
            destination.display()
        )
    })
}

pub(super) fn move_contents(root: &Path, from: &Path, to: &Path) -> Result<()> {
    if !is_portable_relative_path(from, false) {
        bail!("move-contents source must be a safe relative directory");
    }
    if !is_portable_relative_path(to, true) {
        bail!("move-contents destination must be a safe relative directory");
    }
    let source = root.join(from);
    let destination = root.join(to);
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("move-contents source {} does not exist", source.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "move-contents source {} must be a real directory",
            source.display()
        );
    }
    if destination.starts_with(&source) {
        bail!("move-contents destination may not be inside its source");
    }
    fs::create_dir_all(&destination)?;
    let entries = fs::read_dir(&source)?.collect::<std::result::Result<Vec<_>, _>>()?;
    for entry in &entries {
        let target = destination.join(entry.file_name());
        if fs::symlink_metadata(&target).is_ok() {
            bail!("move-contents target {} already exists", target.display());
        }
    }
    for entry in entries {
        let target = destination.join(entry.file_name());
        fs::rename(entry.path(), &target).with_context(|| {
            format!(
                "cannot move {} to {}",
                entry.path().display(),
                target.display()
            )
        })?;
    }
    fs::remove_dir(&source)
        .with_context(|| format!("cannot remove empty source {}", source.display()))?;
    Ok(())
}

fn wildcard_regex(pattern: &str) -> Result<Regex> {
    if !is_portable_filename_pattern(pattern) {
        bail!("rename source must be a non-empty filename pattern");
    }
    let mut expression = String::from("^");
    for character in pattern.chars() {
        match character {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            _ => expression.push_str(&regex::escape(&character.to_string())),
        }
    }
    expression.push('$');
    Ok(Regex::new(&expression).expect("escaped wildcard expression must compile"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{move_contents, rename_one};

    #[test]
    fn renames_exactly_one_nested_match() {
        let directory = tempdir().unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("tool-1.2.3.jar"), "data").unwrap();

        rename_one(directory.path(), "tool-*.jar", Path::new("tool.jar")).unwrap();

        assert_eq!(fs::read_to_string(nested.join("tool.jar")).unwrap(), "data");
        assert!(!nested.join("tool-1.2.3.jar").exists());
    }

    #[test]
    fn refuses_ambiguous_rename_patterns() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("tool-1.jar"), "one").unwrap();
        fs::write(directory.path().join("tool-2.jar"), "two").unwrap();

        let error = rename_one(directory.path(), "tool-*.jar", Path::new("tool.jar")).unwrap_err();

        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn moves_directory_contents_and_removes_the_source() {
        let directory = tempdir().unwrap();
        let release = directory.path().join("release/bin");
        fs::create_dir_all(&release).unwrap();
        fs::write(release.join("tool.exe"), "data").unwrap();

        move_contents(directory.path(), Path::new("release"), Path::new(".")).unwrap();

        assert_eq!(
            fs::read_to_string(directory.path().join("bin/tool.exe")).unwrap(),
            "data"
        );
        assert!(!directory.path().join("release").exists());
    }
}
