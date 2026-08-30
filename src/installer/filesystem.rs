use std::fs::{self, FileType};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::domain::Tool;
use crate::error::UpdaterError;

/// Copies `source` into `destination`. `link_first` trades each regular-file
/// copy for a hard link when the platform and file system support one;
/// unsupported combinations (cross-volume, FAT/exFAT, existing target) fail
/// the link attempt and silently fall back to a byte copy. Direction
/// matters: only link staging-internal trees whose inodes never outlive the
/// run — merge seeding must keep copy semantics so a later overwrite cannot
/// truncate through a shared inode into the rollback backup.
pub(super) fn copy_tree(source: &Path, destination: &Path, link_first: bool) -> Result<()> {
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
            if link_first && let Err(error) = fs::hard_link(entry.path(), &target) {
                tracing::debug!(
                    from = %entry.path().display(),
                    to = %target.display(),
                    error = %error,
                    "hard link unavailable; falling back to copy"
                );
                fs::copy(entry.path(), &target)?;
            } else if !link_first {
                fs::copy(entry.path(), &target)?;
            }
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
    create_symlink_checked(target, std::os::windows::fs::symlink_file(source, target))
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
        create_symlink_checked(target, std::os::windows::fs::symlink_dir(source, target))
    } else {
        create_symlink_checked(target, std::os::windows::fs::symlink_file(source, target))
    }
}

/// ERROR_PRIVILEGE_NOT_HELD: symlink creation on Windows requires Developer
/// Mode or an elevated prompt, so the failure carries the remedy instead of
/// a bare OS error. Other errors pass through unchanged.
#[cfg(windows)]
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

#[cfg(windows)]
fn create_symlink_checked(target: &Path, result: std::io::Result<()>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => Err(
            anyhow::Error::new(error).context(format!(
                "cannot create symlink {}: enable Developer Mode or run as administrator to allow symlink creation",
                target.display()
            )),
        ),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    #[cfg(windows)]
    use super::create_symlink_checked;
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
        copy_tree(&base, &destination, false).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("inner/tool")).unwrap(),
            "ok"
        );
    }

    fn tree_manifest(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut manifest = Vec::new();
        for entry in walkdir::WalkDir::new(root).min_depth(1).sort_by_file_name() {
            let entry = entry.unwrap();
            let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().is_dir() {
                manifest.push((relative, Vec::new()));
            } else {
                manifest.push((relative, fs::read(entry.path()).unwrap()));
            }
        }
        manifest
    }

    #[test]
    fn linked_and_copied_trees_carry_equivalent_manifests() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir_all(source.join("nested/deep")).unwrap();
        fs::write(source.join("root.bin"), b"root").unwrap();
        fs::write(source.join("nested/mid.bin"), b"mid").unwrap();
        fs::write(source.join("nested/deep/leaf.bin"), b"leaf").unwrap();

        let linked = directory.path().join("linked");
        let copied = directory.path().join("copied");
        copy_tree(&source, &linked, true).unwrap();
        copy_tree(&source, &copied, false).unwrap();

        assert_eq!(
            tree_manifest(&linked),
            tree_manifest(&copied),
            "the linked tree must match the copied tree file for file"
        );
        assert_eq!(
            tree_manifest(&linked),
            tree_manifest(&source),
            "the linked tree must reproduce the source contents"
        );

        // Same-inode proof, valid wherever the temp filesystem supports
        // links at all: writing through one name must surface in the other.
        let probe = directory.path().join("link-capability-probe");
        if fs::hard_link(source.join("root.bin"), &probe).is_ok() {
            fs::write(linked.join("root.bin"), b"mutated").unwrap();
            assert_eq!(
                fs::read(source.join("root.bin")).unwrap(),
                b"mutated",
                "link_first must produce hard links sharing the source inode"
            );
            assert_eq!(
                fs::read(copied.join("root.bin")).unwrap(),
                b"root",
                "copy semantics must keep an independent inode"
            );
        }
    }

    #[test]
    fn falls_back_to_copy_when_the_link_target_already_exists() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("tool.bin"), b"fresh").unwrap();
        let destination = directory.path().join("destination");
        fs::create_dir(&destination).unwrap();
        // A pre-existing target makes every hard_link fail with "already
        // exists", forcing the copy fallback on all platforms.
        fs::write(destination.join("tool.bin"), b"stale").unwrap();

        copy_tree(&source, &destination, true).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("tool.bin")).unwrap(),
            "fresh",
            "the failed link must fall back to an overwriting copy"
        );
    }

    #[cfg(windows)]
    #[test]
    fn maps_the_symlink_privilege_error_to_a_remedy() {
        let error = create_symlink_checked(
            std::path::Path::new("link"),
            Err(std::io::Error::from_raw_os_error(1314)),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Developer Mode") && message.contains("administrator"),
            "expected the privilege remedy, got {message}"
        );
        assert!(
            message.contains("os error 1314"),
            "expected the original OS error preserved as source, got {message}"
        );

        let plain = create_symlink_checked(
            std::path::Path::new("link"),
            Err(std::io::Error::from_raw_os_error(5)),
        )
        .unwrap_err();
        assert!(
            !format!("{plain:#}").contains("Developer Mode"),
            "unrelated errors must pass through unchanged"
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
