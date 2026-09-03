use std::fs::{self, FileType};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::archive::extract::ensure_bounded_link_target;
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

/// strip_single_root 提升与多产物合并会改变链接的相对边界，解压时的
/// 界内校验到此不再可靠；以最终组装目录为根复验全部符号链接。
pub(super) fn verify_staged_links(root: &Path) -> Result<()> {
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry?;
        if !entry.file_type().is_symlink() {
            continue;
        }
        let member = entry.path().strip_prefix(root)?;
        let target = fs::read_link(entry.path())?;
        ensure_bounded_link_target(
            root,
            root,
            &root_canonical,
            "symbolic link",
            member,
            &target,
        )?;
    }
    Ok(())
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
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub(super) fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

pub(super) fn remove_file_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => anyhow::bail!("refusing to remove non-symlink {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
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
    #[cfg(unix)]
    use super::{path_exists, remove_path};

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

    #[cfg(unix)]
    #[test]
    fn handles_missing_paths_and_dangling_symlinks() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("missing");
        let dangling = directory.path().join("dangling");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();

        assert!(!path_exists(&missing).unwrap());
        assert!(path_exists(&dangling).unwrap());
        remove_path(&dangling).unwrap();
        remove_path(&missing).unwrap();
        assert!(!path_exists(&dangling).unwrap());
    }
}
