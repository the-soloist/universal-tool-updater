use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub(super) const LOCK_FILENAME: &str = ".updater-self-update.lock";

pub(super) struct UpdateLock {
    _file: File,
    #[cfg(any(windows, test))]
    pub(super) path: PathBuf,
}

impl UpdateLock {
    pub(super) fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(LOCK_FILENAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open self-update lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!("another self-update is already running");
            }
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("cannot lock self-update lock {}", path.display()));
            }
        }
        file.set_len(0)
            .with_context(|| format!("cannot reset self-update lock {}", path.display()))?;
        if let Err(error) = writeln!(
            file,
            "pid={} version={}",
            std::process::id(),
            env!("CARGO_PKG_VERSION")
        )
        .and_then(|()| file.sync_all())
        {
            return Err(error)
                .with_context(|| format!("cannot initialize self-update lock {}", path.display()));
        }
        Ok(Self {
            _file: file,
            #[cfg(any(windows, test))]
            path,
        })
    }

    #[cfg(any(windows, test))]
    pub(super) fn acquire_for_helper(directory: &Path) -> Result<Self> {
        let path = directory.join(LOCK_FILENAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open self-update lock {}", path.display()))?;
        file.lock()
            .with_context(|| format!("cannot acquire self-update lock {}", path.display()))?;
        Ok(Self { _file: file, path })
    }

    #[cfg(any(windows, test))]
    pub(super) fn release_for_handoff(&self) -> Result<()> {
        self._file
            .unlock()
            .with_context(|| format!("cannot hand off self-update lock {}", self.path.display()))
    }
}
