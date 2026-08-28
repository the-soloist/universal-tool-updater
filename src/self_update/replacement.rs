use std::fs;
use std::path::Path;

#[cfg(windows)]
use anyhow::bail;
use anyhow::{Context, Result};
use semver::Version;
use tempfile::TempDir;

use super::UpdateLock;
#[cfg(windows)]
use super::WORK_DIRECTORY_PREFIX;

#[cfg(windows)]
const HELPER_READY_FILENAME: &str = "helper.ready";

pub(super) enum InstallOutcome {
    Completed,
    #[cfg(windows)]
    Scheduled,
}

#[cfg(all(unix, not(windows)))]
pub(super) fn install(
    target: &Path,
    candidate: &Path,
    work_dir: TempDir,
    _version: &Version,
    _lock: &mut UpdateLock,
) -> Result<InstallOutcome> {
    let backup = work_dir.path().join("updater.previous");
    fs::copy(target, &backup).with_context(|| {
        format!(
            "cannot back up the current updater {} to {}",
            target.display(),
            backup.display()
        )
    })?;
    replace_unix(target, candidate)?;
    Ok(InstallOutcome::Completed)
}

#[cfg(all(unix, not(windows)))]
fn replace_unix(target: &Path, candidate: &Path) -> Result<()> {
    fs::rename(candidate, target).with_context(|| {
        format!(
            "cannot atomically replace updater {} with {}",
            target.display(),
            candidate.display()
        )
    })
}

#[cfg(windows)]
pub(super) fn install(
    target: &Path,
    candidate: &Path,
    work_dir: TempDir,
    version: &Version,
    lock: &mut UpdateLock,
) -> Result<InstallOutcome> {
    use std::process::{Command, Stdio};

    let target_parent = target
        .parent()
        .context("current updater has no parent directory")?;
    let version = version.to_string();
    let helper = work_dir.path().join("updater-self-replace.exe");
    fs::copy(target, &helper).with_context(|| {
        format!(
            "cannot create the Windows self-update helper {}",
            helper.display()
        )
    })?;
    let work_path = work_dir.keep();
    let spawn = Command::new(&helper)
        .arg("__self-replace")
        .arg("--target")
        .arg(target)
        .arg("--candidate")
        .arg(candidate)
        .arg("--version")
        .arg(&version)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    match spawn {
        Ok(mut child) => {
            if let Err(error) = wait_for_helper_ready(&mut child, &work_path) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&work_path);
                return Err(error);
            }
            if let Err(error) = super::status::write_scheduled(target_parent, &version) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&work_path);
                return Err(error);
            }
            if let Err(error) = lock.release_for_handoff() {
                if let Err(status_error) =
                    super::status::write_failure(target_parent, &version, format!("{error:#}"))
                {
                    tracing::error!(error = %status_error, "cannot persist failed self-update result");
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&work_path);
                return Err(error);
            }
            drop(child);
            Ok(InstallOutcome::Scheduled)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&work_path);
            Err(error).with_context(|| {
                format!(
                    "cannot launch the Windows self-update helper {}",
                    helper.display()
                )
            })
        }
    }
}

#[cfg(windows)]
fn wait_for_helper_ready(child: &mut std::process::Child, work_dir: &Path) -> Result<()> {
    use std::thread;
    use std::time::Duration;

    let ready = work_dir.join(HELPER_READY_FILENAME);
    for _ in 0..100 {
        if ready.is_file() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("cannot inspect Windows self-update helper status")?
        {
            bail!("Windows self-update helper exited before lock handoff with status {status}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for the Windows self-update helper to initialize")
}

#[cfg(windows)]
pub(super) fn replace_helper(target: &Path, candidate: &Path, version: &str) -> Result<()> {
    let helper = std::env::current_exe().context("cannot determine self-update helper path")?;
    let work_dir = helper
        .parent()
        .context("self-update helper has no parent directory")?;
    validate_helper_layout(&helper, work_dir, target, candidate)?;
    let ready = work_dir.join(HELPER_READY_FILENAME);
    fs::write(&ready, b"ready").with_context(|| {
        format!(
            "cannot signal Windows self-update helper readiness at {}",
            ready.display()
        )
    })?;
    let target_parent = target.parent().expect("validated target has a parent");
    let _lock = UpdateLock::acquire_for_helper(target_parent)?;
    let _ = fs::remove_file(&ready);
    let backup = work_dir.join("updater.previous.exe");
    if let Err(error) = wait_and_replace_windows(target, candidate, &backup) {
        if let Err(status_error) =
            super::status::write_failure(target_parent, version, format!("{error:#}"))
        {
            tracing::error!(error = %status_error, "cannot persist failed self-update result");
        }
        if let Ok(cleanup) = spawn_cleanup(target, work_dir) {
            drop(cleanup);
        }
        return Err(error);
    }

    let cleanup = match spawn_cleanup(target, work_dir) {
        Ok(cleanup) => cleanup,
        Err(error) => {
            if let Err(status_error) =
                super::status::write_failure(target_parent, version, format!("{error:#}"))
            {
                tracing::error!(error = %status_error, "cannot persist failed self-update result");
            }
            return Err(error);
        }
    };
    drop(cleanup);
    super::status::write_success(target_parent, version)?;
    println!("updater updated successfully to v{version}");
    Ok(())
}

#[cfg(windows)]
fn spawn_cleanup(target: &Path, work_dir: &Path) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};

    Command::new(target)
        .arg("__self-cleanup")
        .arg("--work-dir")
        .arg(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "updater could not start self-update cleanup from {}",
                target.display()
            )
        })
}

#[cfg(windows)]
pub(super) fn cleanup_helper(work_dir: &Path) -> Result<()> {
    use std::io::ErrorKind;
    use std::thread;
    use std::time::Duration;

    validate_cleanup_directory(work_dir)?;
    let mut last_error = None;
    for _ in 0..150 {
        match fs::remove_dir_all(work_dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let error = last_error.expect("cleanup retry loop records every failure");
    Err(error).with_context(|| {
        format!(
            "cannot remove completed self-update workspace {}",
            work_dir.display()
        )
    })
}

#[cfg(windows)]
fn validate_helper_layout(
    helper: &Path,
    work_dir: &Path,
    target: &Path,
    candidate: &Path,
) -> Result<()> {
    let work_dir = fs::canonicalize(work_dir).with_context(|| {
        format!(
            "cannot resolve self-update workspace {}",
            work_dir.display()
        )
    })?;
    let helper = fs::canonicalize(helper)
        .with_context(|| format!("cannot resolve self-update helper {}", helper.display()))?;
    let target = fs::canonicalize(target)
        .with_context(|| format!("cannot resolve current updater {}", target.display()))?;
    let candidate = fs::canonicalize(candidate)
        .with_context(|| format!("cannot resolve candidate updater {}", candidate.display()))?;
    let target_parent = target
        .parent()
        .context("current updater has no parent directory")?;
    let workspace_parent = work_dir
        .parent()
        .context("self-update workspace has no parent directory")?;
    let workspace_name = work_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("self-update workspace name is not valid UTF-8")?;
    if helper.parent() != Some(work_dir.as_path())
        || workspace_parent != target_parent
        || !workspace_name.starts_with(WORK_DIRECTORY_PREFIX)
        || !candidate.starts_with(&work_dir)
    {
        bail!("refusing an invalid Windows self-update helper layout");
    }
    Ok(())
}

#[cfg(windows)]
fn validate_cleanup_directory(work_dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(work_dir).with_context(|| {
        format!(
            "cannot inspect self-update workspace {}",
            work_dir.display()
        )
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "refusing to clean non-directory self-update workspace {}",
            work_dir.display()
        );
    }
    let current = std::env::current_exe().context("cannot determine updated updater path")?;
    let current_parent = current
        .parent()
        .context("updated updater has no parent directory")?;
    let expected_parent = fs::canonicalize(current_parent).with_context(|| {
        format!(
            "cannot resolve updater directory {}",
            current_parent.display()
        )
    })?;
    let actual_parent = work_dir
        .parent()
        .context("self-update workspace has no parent directory")?;
    let actual_parent = fs::canonicalize(actual_parent).with_context(|| {
        format!(
            "cannot resolve self-update workspace parent {}",
            actual_parent.display()
        )
    })?;
    let name = work_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("self-update workspace name is not valid UTF-8")?;
    if actual_parent != expected_parent || !name.starts_with(WORK_DIRECTORY_PREFIX) {
        bail!(
            "refusing to clean invalid self-update workspace {}",
            work_dir.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn wait_and_replace_windows(target: &Path, candidate: &Path, backup: &Path) -> Result<()> {
    use std::thread;
    use std::time::Duration;

    let mut last_error = None;
    for _ in 0..150 {
        match fs::rename(target, backup) {
            Ok(()) => {
                return match fs::rename(candidate, target) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        let rollback = fs::rename(backup, target);
                        match rollback {
                            Ok(()) => Err(error).with_context(|| {
                                format!(
                                    "cannot install candidate updater {}; restored the previous updater",
                                    candidate.display()
                                )
                            }),
                            Err(rollback) => bail!(
                                "cannot install candidate updater {}: {error}; rollback from {} also failed: {rollback}",
                                candidate.display(),
                                backup.display()
                            ),
                        }
                    }
                };
            }
            Err(error) if is_retryable_windows_replace_error(&error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot move current updater {} out of the way",
                        target.display()
                    )
                });
            }
        }
    }
    let error = last_error.expect("replacement retry loop records every failure");
    Err(error).with_context(|| {
        format!(
            "timed out waiting for current updater {} to exit",
            target.display()
        )
    })
}

#[cfg(windows)]
fn is_retryable_windows_replace_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    matches!(error.raw_os_error(), Some(32 | 33))
        || matches!(
            error.kind(),
            ErrorKind::PermissionDenied | ErrorKind::WouldBlock
        )
}

#[cfg(test)]
mod tests {
    #[cfg(any(unix, windows))]
    use std::fs;

    #[cfg(any(unix, windows))]
    use tempfile::tempdir;

    #[cfg(all(unix, not(windows)))]
    use super::replace_unix;
    #[cfg(windows)]
    use super::{is_retryable_windows_replace_error, wait_and_replace_windows};

    #[cfg(all(unix, not(windows)))]
    #[test]
    fn atomically_replaces_an_existing_unix_binary() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("updater");
        let candidate = directory.path().join("candidate");
        fs::write(&target, "old").unwrap();
        fs::write(&candidate, "new").unwrap();
        replace_unix(&target, &candidate).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(!candidate.exists());
    }

    #[cfg(windows)]
    #[test]
    fn replaces_a_released_windows_binary() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("updater.exe");
        let candidate = directory.path().join("candidate.exe");
        let backup = directory.path().join("updater.previous.exe");
        fs::write(&target, "old").unwrap();
        fs::write(&candidate, "new").unwrap();
        wait_and_replace_windows(&target, &candidate, &backup).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
    }

    #[cfg(windows)]
    #[test]
    fn restores_windows_binary_when_candidate_move_fails() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("updater.exe");
        let missing = directory.path().join("missing.exe");
        let backup = directory.path().join("updater.previous.exe");
        fs::write(&target, "old").unwrap();
        assert!(wait_and_replace_windows(&target, &missing, &backup).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert!(!backup.exists());
    }

    #[cfg(windows)]
    #[test]
    fn retries_windows_sharing_and_lock_violations() {
        assert!(is_retryable_windows_replace_error(
            &std::io::Error::from_raw_os_error(32)
        ));
        assert!(is_retryable_windows_replace_error(
            &std::io::Error::from_raw_os_error(33)
        ));
    }
}
