use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use semver::Version;
use walkdir::WalkDir;

pub(super) fn current_updater() -> Result<PathBuf> {
    let path = std::env::current_exe().context("cannot determine the current updater path")?;
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect current updater {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!(
            "refusing to replace current updater because {} is not a regular file",
            path.display()
        );
    }
    Ok(path)
}

pub(super) fn find_candidate(directory: &Path) -> Result<PathBuf> {
    let expected = if cfg!(windows) {
        "updater.exe"
    } else {
        "updater"
    };
    let mut files = Vec::new();
    for entry in WalkDir::new(directory).follow_links(false) {
        let entry = entry.with_context(|| {
            format!(
                "cannot inspect extracted self-update directory {}",
                directory.display()
            )
        })?;
        if entry.file_type().is_symlink() {
            bail!(
                "self-update archive contains a symbolic link: {}",
                entry.path().display()
            );
        }
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    if files.len() != 1 {
        bail!(
            "self-update archive must contain exactly one file named {expected:?}; found {} files",
            files.len()
        );
    }
    let candidate = files.pop().expect("one candidate was verified");
    if candidate.file_name().and_then(|name| name.to_str()) != Some(expected) {
        bail!(
            "self-update archive contains {}, expected {expected:?}",
            candidate.display()
        );
    }
    if fs::metadata(&candidate)
        .with_context(|| format!("cannot inspect candidate updater {}", candidate.display()))?
        .len()
        == 0
    {
        bail!("candidate updater {} is empty", candidate.display());
    }
    Ok(candidate)
}

#[cfg(unix)]
pub(super) fn prepare_candidate(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions).with_context(|| {
        format!(
            "cannot make candidate updater executable at {}",
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn prepare_candidate(_path: &Path) -> Result<()> {
    Ok(())
}

pub(super) fn verify_candidate(path: &Path, expected: &Version) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("cannot execute candidate updater {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "candidate updater {} failed its --version check with status {}",
            path.display(),
            output.status
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("candidate updater returned a non-UTF-8 version string")?;
    let expected_output = format!("updater {expected}");
    if stdout.trim() != expected_output {
        bail!(
            "candidate updater reported version {:?}, expected {:?}",
            stdout.trim(),
            expected_output
        );
    }
    Ok(())
}
