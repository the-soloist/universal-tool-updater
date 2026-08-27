use std::fs::{self, FileType};
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use crate::domain::Tool;
use crate::error::UpdaterError;

pub(super) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in WalkDir::new(source).follow_links(false).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            fs::create_dir_all(&target)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        } else if file_type.is_symlink() {
            let linked = fs::read_link(entry.path())?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            create_symlink_by_target(
                &linked,
                &target,
                fs::metadata(entry.path())
                    .ok()
                    .map(|metadata| metadata.file_type()),
            )?;
        }
    }
    Ok(())
}

pub(super) fn single_directory_base(directory: &Path) -> Result<PathBuf> {
    let entries = fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
    if entries.len() == 1 && entries[0].file_type()?.is_dir() {
        Ok(entries[0].path())
    } else {
        Ok(directory.to_path_buf())
    }
}

pub(super) fn apply_executable_bits(tool: &Tool, root: &Path) -> Result<()> {
    for relative in &tool.install.executable {
        let path = root.join(relative);
        if !path.is_file() {
            return Err(UpdaterError::Installation {
                tool: tool.id.clone(),
                message: format!("executable file {} does not exist", path.display()),
            }
            .into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            fs::set_permissions(&path, permissions)?;
        }
    }
    Ok(())
}

pub(super) fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)?;
    } else if path.exists() || path.is_symlink() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn create_file_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, target)?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn create_file_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(source, target)?;
    Ok(())
}

#[cfg(unix)]
fn create_symlink_by_target(
    source: &Path,
    target: &Path,
    _file_type: Option<FileType>,
) -> Result<()> {
    std::os::unix::fs::symlink(source, target)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink_by_target(
    source: &Path,
    target: &Path,
    file_type: Option<FileType>,
) -> Result<()> {
    if file_type.is_some_and(|kind| kind.is_dir()) {
        std::os::windows::fs::symlink_dir(source, target)?;
    } else {
        std::os::windows::fs::symlink_file(source, target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{copy_tree, single_directory_base};

    #[test]
    fn copies_and_flattens_single_directory_trees() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source/outer/inner");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("tool"), "ok").unwrap();
        let base = single_directory_base(&directory.path().join("source")).unwrap();
        let destination = directory.path().join("destination");
        copy_tree(&base, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("inner/tool")).unwrap(),
            "ok"
        );
    }
}
