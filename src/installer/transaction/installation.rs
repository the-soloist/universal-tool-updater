use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::installation_backup_path;

use super::super::filesystem::{path_exists, remove_path};
use super::{combine_rollbacks, with_rollback};

pub(super) struct InstallationTransaction {
    destination: Backup,
    version: Option<Backup>,
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
            destination_installed: false,
            version_installed: false,
        })
    }

    pub(super) fn install(&mut self, ready: &Path, external_version: Option<&Path>) -> Result<()> {
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
        Ok(())
    }

    pub(super) fn rollback(self) -> Result<()> {
        let version_result = self
            .version
            .map(|version| version.restore_after_install(self.version_installed));
        let destination_result = self
            .destination
            .restore_after_install(self.destination_installed);
        combine_rollbacks(version_result, Some(destination_result))
    }

    pub(super) fn finish(self) -> Result<()> {
        let destination_result = self.destination.discard();
        let version_result = self.version.map(Backup::discard);
        combine_rollbacks(Some(destination_result), version_result)
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
        if installed {
            remove_path(&self.original).with_context(|| {
                format!(
                    "cannot remove failed installation {}",
                    self.original.display()
                )
            })?;
        }
        if self.existed {
            fs::rename(&self.backup, &self.original).with_context(|| {
                format!(
                    "cannot restore backup {} to {}",
                    self.backup.display(),
                    self.original.display()
                )
            })?;
        }
        Ok(())
    }

    fn discard(self) -> Result<()> {
        if self.existed {
            remove_path(&self.backup)
                .with_context(|| format!("cannot remove backup {}", self.backup.display()))?;
        }
        Ok(())
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
        return recover_stale_backup(destination, &backup_path(destination)?);
    };
    let destination_backup = backup_path(destination)?;
    let version_backup = backup_path(version)?;
    let has_destination_backup = path_exists(&destination_backup)?;
    let has_version_backup = path_exists(&version_backup)?;
    if !has_destination_backup && !has_version_backup {
        return Ok(());
    }

    let has_destination = path_exists(destination)?;
    let has_version = path_exists(version)?;
    match (has_destination_backup, has_version_backup) {
        (true, true) if has_destination && has_version => {
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
    Ok(())
}

fn backup_path(path: &Path) -> Result<PathBuf> {
    installation_backup_path(path)
        .with_context(|| format!("installation path {} has no filename", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{InstallationTransaction, backup_path};

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
        transaction.install(&ready, None).unwrap();
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
        transaction.install(&ready, Some(&staged_version)).unwrap();
        transaction.rollback().unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("old.txt")).unwrap(),
            "old"
        );
        assert_eq!(fs::read_to_string(version).unwrap(), "v1\n");
        assert!(!destination.join("new.txt").exists());
        assert!(!destination.join("next.txt").exists());
    }
}
