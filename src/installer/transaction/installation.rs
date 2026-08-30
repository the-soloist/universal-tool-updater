use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::installation_backup_path;

use super::super::filesystem::{path_exists, remove_path};
use super::{combine_rollbacks, with_rollback};

pub(super) struct InstallationTransaction {
    destination: Backup,
    version: Option<Backup>,
    marker: Option<PathBuf>,
    destination_installed: bool,
    version_installed: bool,
}

impl InstallationTransaction {
    pub(super) fn begin(destination: &Path, version: Option<&Path>) -> Result<Self> {
        // 遗留备份说明上次进程在事务中途停止；先恢复它再创建新备份，确保本次安装从一致状态开始。
        recover_interrupted_installation(destination, version)?;
        let destination = Backup::take(destination)?;
        let version = match version.map(Backup::take).transpose() {
            Ok(version) => version,
            Err(error) => return Err(with_rollback(error, destination.restore())),
        };
        Ok(Self {
            destination,
            version,
            marker: None,
            destination_installed: false,
            version_installed: false,
        })
    }

    pub(super) fn install(
        &mut self,
        tool_id: &str,
        version: &str,
        ready: &Path,
        external_version: Option<&Path>,
    ) -> Result<()> {
        fs::rename(ready, &self.destination.original).with_context(|| {
            format!(
                "cannot commit installation to {}",
                self.destination.original.display()
            )
        })?;
        self.destination_installed = true;

        if let (Some(staged), Some(version)) = (external_version, &self.version) {
            fs::rename(staged, &version.original).with_context(|| {
                format!(
                    "cannot commit version marker to {}",
                    version.original.display()
                )
            })?;
            self.version_installed = true;
        }
        // Two-phase commit: the marker is written only after both renames
        // succeeded, so its presence tells crash recovery that the on-disk
        // installation is committed rather than crashed mid-way.
        let marker = committed_marker_path(&self.destination.original).with_context(|| {
            format!(
                "installation path {} has no filename",
                self.destination.original.display()
            )
        })?;
        fs::write(&marker, format!("{tool_id}\n{version}\n"))
            .with_context(|| format!("cannot write the committed marker {}", marker.display()))?;
        self.marker = Some(marker);
        Ok(())
    }

    pub(super) fn rollback(self) -> Result<()> {
        let version_result = self
            .version
            .map(|version| version.restore_after_install(self.version_installed));
        let destination_result = self
            .destination
            .restore_after_install(self.destination_installed);
        remove_marker_quietly(self.marker.as_deref());
        combine_rollbacks(version_result, Some(destination_result))
    }

    pub(super) fn finish(self) -> Result<()> {
        let Self {
            destination,
            version,
            marker,
            ..
        } = self;
        destination.discard();
        if let Some(version) = version {
            version.discard();
        }
        remove_marker_quietly(marker.as_deref());
        Ok(())
    }
}

struct Backup {
    original: PathBuf,
    backup: PathBuf,
    existed: bool,
}

impl Backup {
    fn take(original: &Path) -> Result<Self> {
        let backup = backup_path(original)?;
        recover_stale_backup(original, &backup)?;
        let existed = path_exists(original)?;
        if existed {
            fs::rename(original, &backup).with_context(|| {
                format!(
                    "cannot back up {} to {}",
                    original.display(),
                    backup.display()
                )
            })?;
        }
        Ok(Self {
            original: original.to_path_buf(),
            backup,
            existed,
        })
    }

    fn restore(self) -> Result<()> {
        self.restore_after_install(false)
    }

    fn restore_after_install(self, installed: bool) -> Result<()> {
        // Removing the failed installation and restoring the backup are both
        // attempted: bailing on the first failure would leave the old version
        // stranded in the backup directory without any pointer to it.
        let removal = if installed {
            remove_path(&self.original).with_context(|| {
                format!(
                    "cannot remove failed installation {}",
                    self.original.display()
                )
            })
        } else {
            Ok(())
        };
        let restore = if self.existed {
            fs::rename(&self.backup, &self.original).with_context(|| {
                format!(
                    "cannot restore backup {} to {}",
                    self.backup.display(),
                    self.original.display()
                )
            })
        } else {
            Ok(())
        };
        let restore_succeeded = restore.is_ok();
        let failures = [removal, restore]
            .into_iter()
            .filter_map(|result| result.err().map(|error| format!("{error:#}")))
            .collect::<Vec<_>>();
        if failures.is_empty() {
            return Ok(());
        }
        if !self.existed {
            anyhow::bail!("{}", failures.join("; "));
        }
        if restore_succeeded {
            anyhow::bail!(
                "{}; the previous version was restored from the backup {}",
                failures.join("; "),
                self.backup.display()
            );
        }
        anyhow::bail!(
            "{}; the previous version is still in the backup {}",
            failures.join("; "),
            self.backup.display()
        );
    }

    /// Backup removal after a successful commit is best-effort: on Windows the
    /// running tool may still hold locks, so an undeletable backup is kept
    /// (its `.utu-backup` name keeps crash recovery working) instead of
    /// failing an installation that already committed.
    fn discard(self) {
        if self.existed
            && let Err(error) = remove_path(&self.backup)
        {
            tracing::warn!(
                backup = %self.backup.display(),
                error = %error,
                "cannot remove the installation backup after committing; keeping it for crash recovery"
            );
        }
    }
}

fn recover_stale_backup(original: &Path, backup: &Path) -> Result<()> {
    if !path_exists(backup)? {
        return Ok(());
    }
    if path_exists(original)? {
        remove_path(backup)
            .with_context(|| format!("cannot remove stale backup {}", backup.display()))
    } else {
        fs::rename(backup, original).with_context(|| {
            format!(
                "cannot recover interrupted installation {} from {}",
                original.display(),
                backup.display()
            )
        })
    }
}

fn recover_interrupted_installation(destination: &Path, version: Option<&Path>) -> Result<()> {
    let Some(version) = version else {
        recover_stale_backup(destination, &backup_path(destination)?)?;
        remove_committed_marker(destination)?;
        return Ok(());
    };
    let destination_backup = backup_path(destination)?;
    let version_backup = backup_path(version)?;
    let has_destination_backup = path_exists(&destination_backup)?;
    let has_version_backup = path_exists(&version_backup)?;
    if !has_destination_backup && !has_version_backup {
        remove_committed_marker(destination)?;
        return Ok(());
    }

    let has_destination = path_exists(destination)?;
    let has_version = path_exists(version)?;
    // Two backups with both targets present only prove a committed
    // installation when the committed marker survived; without it the state
    // is suspicious and the backups are restored to force a reinstall.
    let marker_present = match committed_marker_path(destination) {
        Some(marker) => path_exists(&marker)?,
        None => false,
    };
    let committed = has_destination && has_version && marker_present;
    match (has_destination_backup, has_version_backup) {
        (true, true) if committed => {
            remove_path(&destination_backup)?;
            remove_path(&version_backup)?;
        }
        (true, true) => {
            if has_destination {
                remove_path(destination)?;
            }
            if has_version {
                remove_path(version)?;
            }
            fs::rename(&destination_backup, destination).with_context(|| {
                format!(
                    "cannot recover interrupted installation {} from {}",
                    destination.display(),
                    destination_backup.display()
                )
            })?;
            fs::rename(&version_backup, version).with_context(|| {
                format!(
                    "cannot recover interrupted version marker {} from {}",
                    version.display(),
                    version_backup.display()
                )
            })?;
        }
        (true, false) => recover_stale_backup(destination, &destination_backup)?,
        (false, true) => recover_stale_backup(version, &version_backup)?,
        (false, false) => unreachable!("at least one backup exists"),
    }
    remove_committed_marker(destination)?;
    Ok(())
}

fn backup_path(path: &Path) -> Result<PathBuf> {
    installation_backup_path(path)
        .with_context(|| format!("installation path {} has no filename", path.display()))
}

fn committed_marker_path(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".utu-committed");
    Some(path.with_file_name(name))
}

fn remove_committed_marker(destination: &Path) -> Result<()> {
    let Some(marker) = committed_marker_path(destination) else {
        return Ok(());
    };
    if let Err(error) = fs::remove_file(&marker)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("cannot remove committed marker {}", marker.display()));
    }
    Ok(())
}

fn remove_marker_quietly(marker: Option<&Path>) {
    let Some(marker) = marker else {
        return;
    };
    if let Err(error) = fs::remove_file(marker)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(
            marker = %marker.display(),
            error = %error,
            "cannot remove the committed installation marker"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{InstallationTransaction, backup_path, committed_marker_path};

    #[test]
    fn recovers_an_interrupted_backup_before_starting_a_new_transaction() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo");
        let backup = backup_path(&destination).unwrap();
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("old.txt"), "old").unwrap();
        let ready = directory.path().join("ready");
        fs::create_dir(&ready).unwrap();
        fs::write(ready.join("new.txt"), "new").unwrap();

        let mut transaction = InstallationTransaction::begin(&destination, None).unwrap();
        transaction.install("demo", "v2", &ready, None).unwrap();
        transaction.rollback().unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!destination.join("new.txt").exists());
        assert!(!backup.exists());
    }

    #[test]
    fn rolls_back_a_destination_committed_before_its_version_marker() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo/release");
        let version = directory.path().join("Demo/.version");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("new.txt"), "new").unwrap();
        let destination_backup = backup_path(&destination).unwrap();
        fs::create_dir(&destination_backup).unwrap();
        fs::write(destination_backup.join("old.txt"), "old").unwrap();
        let version_backup = backup_path(&version).unwrap();
        fs::write(&version_backup, "v1\n").unwrap();
        let ready = directory.path().join("ready");
        fs::create_dir(&ready).unwrap();
        fs::write(ready.join("next.txt"), "next").unwrap();
        let staged_version = directory.path().join("staged-version");
        fs::write(&staged_version, "v3\n").unwrap();

        let mut transaction = InstallationTransaction::begin(&destination, Some(&version)).unwrap();
        transaction
            .install("demo", "v3", &ready, Some(&staged_version))
            .unwrap();
        transaction.rollback().unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("old.txt")).unwrap(),
            "old"
        );
        assert_eq!(fs::read_to_string(version).unwrap(), "v1\n");
        assert!(!destination.join("new.txt").exists());
        assert!(!destination.join("next.txt").exists());
    }

    #[test]
    fn recovers_backups_when_a_committed_installation_lacks_the_marker() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo/release");
        let version = directory.path().join("Demo/.version");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("new.txt"), "new").unwrap();
        fs::write(&version, "v2\n").unwrap();
        let destination_backup = backup_path(&destination).unwrap();
        fs::create_dir(&destination_backup).unwrap();
        fs::write(destination_backup.join("old.txt"), "old").unwrap();
        let version_backup = backup_path(&version).unwrap();
        fs::write(&version_backup, "v1\n").unwrap();

        // Simulate a crash after both commits but before the marker write.
        InstallationTransaction::begin(&destination, Some(&version)).unwrap();

        // begin() re-took backups after recovery; their content proves that
        // the pre-crash backups were restored, not discarded.
        assert_eq!(
            fs::read_to_string(destination_backup.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!destination_backup.join("new.txt").exists());
        assert_eq!(
            fs::read_to_string(version_backup).unwrap(),
            "v1\n",
            "the suspicious version marker must be rolled back to the backup"
        );
        assert!(
            !committed_marker_path(&destination).unwrap().exists(),
            "recovery must clear the committed marker"
        );
    }

    #[test]
    fn discards_backups_when_a_committed_installation_left_its_marker() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo/release");
        let version = directory.path().join("Demo/.version");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("new.txt"), "new").unwrap();
        fs::write(&version, "v2\n").unwrap();
        let destination_backup = backup_path(&destination).unwrap();
        fs::create_dir(&destination_backup).unwrap();
        fs::write(destination_backup.join("old.txt"), "old").unwrap();
        let version_backup = backup_path(&version).unwrap();
        fs::write(&version_backup, "v1\n").unwrap();
        let marker = committed_marker_path(&destination).unwrap();
        fs::write(&marker, "demo\nv2\n").unwrap();

        // Simulate a committed installation that crashed before finish().
        InstallationTransaction::begin(&destination, Some(&version)).unwrap();

        // begin() re-took backups from the committed content, proving the
        // crash-time backups were discarded instead of restored.
        assert_eq!(
            fs::read_to_string(destination_backup.join("new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(version_backup).unwrap(),
            "v2\n",
            "the committed version marker must survive recovery"
        );
        assert!(!marker.exists());
    }

    #[cfg(windows)]
    #[test]
    fn rollback_reports_the_backup_path_when_removal_of_the_failed_installation_fails() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("old.txt"), "old").unwrap();
        let backup = backup_path(&destination).unwrap();
        let ready = directory.path().join("ready");
        fs::create_dir(&ready).unwrap();
        fs::write(ready.join("new.txt"), "new").unwrap();

        let mut transaction = InstallationTransaction::begin(&destination, None).unwrap();
        transaction.install("demo", "v2", &ready, None).unwrap();
        // Simulate a running tool inside the committed installation: an
        // exclusive open with no sharing blocks deletion on Windows.
        let _lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(destination.join("new.txt"))
            .unwrap();

        let error = transaction.rollback().unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains(&backup.display().to_string()),
            "expected the backup path in {message:?}"
        );
        assert!(
            backup.exists(),
            "the backup must survive a failed rollback so the old version is recoverable"
        );
        assert!(destination.join("new.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn finish_keeps_an_undeletable_backup_instead_of_failing_the_installation() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo/.version");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, "v1\n").unwrap();
        let backup = backup_path(&destination).unwrap();
        let ready = directory.path().join("ready");
        fs::write(&ready, "v2\n").unwrap();

        let mut transaction = InstallationTransaction::begin(&destination, None).unwrap();
        transaction.install("demo", "v2", &ready, None).unwrap();
        // Simulate a running tool holding the backup: an exclusive open with
        // no sharing blocks deletion on Windows.
        let _lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)
            .unwrap();

        transaction.finish().unwrap();

        assert_eq!(fs::read_to_string(&destination).unwrap(), "v2\n");
        assert!(
            backup.exists(),
            "the undeletable backup must survive for crash recovery"
        );
    }
}
