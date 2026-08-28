#[cfg(any(windows, test))]
use std::io::Write;
use std::path::Path;
#[cfg(any(windows, test))]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
#[cfg(any(windows, test))]
use serde::Serialize;
#[cfg(any(windows, test))]
use tempfile::NamedTempFile;

pub(super) const RESULT_FILENAME: &str = ".updater-self-update-result.toml";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[cfg_attr(any(windows, test), derive(Serialize))]
#[serde(rename_all = "lowercase")]
pub(super) enum StoredStatus {
    Scheduled,
    Success,
    Failed,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(any(windows, test), derive(Serialize))]
pub(super) struct StoredResult {
    pub schema_version: u32,
    pub status: StoredStatus,
    pub version: String,
    pub updated_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(any(windows, test))]
pub(super) fn write_scheduled(directory: &Path, version: &str) -> Result<()> {
    write(
        directory,
        &StoredResult {
            schema_version: 1,
            status: StoredStatus::Scheduled,
            version: version.to_owned(),
            updated_at_unix_ms: now_unix_ms()?,
            message: Some("verified update is waiting for the parent process to exit".to_owned()),
        },
    )
}

#[cfg(any(windows, test))]
pub(super) fn write_success(directory: &Path, version: &str) -> Result<()> {
    write(
        directory,
        &StoredResult {
            schema_version: 1,
            status: StoredStatus::Success,
            version: version.to_owned(),
            updated_at_unix_ms: now_unix_ms()?,
            message: None,
        },
    )
}

#[cfg(any(windows, test))]
pub(super) fn write_failure(directory: &Path, version: &str, message: String) -> Result<()> {
    write(
        directory,
        &StoredResult {
            schema_version: 1,
            status: StoredStatus::Failed,
            version: version.to_owned(),
            updated_at_unix_ms: now_unix_ms()?,
            message: Some(message),
        },
    )
}

pub(super) fn read(directory: &Path) -> Result<Option<StoredResult>> {
    let path = directory.join(RESULT_FILENAME);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read self-update result {}", path.display()));
        }
    };
    let result = toml::from_str::<StoredResult>(&contents)
        .with_context(|| format!("cannot parse self-update result {}", path.display()))?;
    if result.schema_version != 1 {
        anyhow::bail!(
            "unsupported self-update result schema {} in {}",
            result.schema_version,
            path.display()
        );
    }
    Ok(Some(result))
}

#[cfg(any(windows, test))]
fn write(directory: &Path, result: &StoredResult) -> Result<()> {
    let destination = directory.join(RESULT_FILENAME);
    let contents = toml::to_string(result).context("cannot serialize self-update result")?;
    let mut temporary = NamedTempFile::new_in(directory).with_context(|| {
        format!(
            "cannot create temporary self-update result beside {}",
            destination.display()
        )
    })?;
    temporary.write_all(contents.as_bytes()).with_context(|| {
        format!(
            "cannot write temporary self-update result for {}",
            destination.display()
        )
    })?;
    temporary.as_file().sync_all().with_context(|| {
        format!(
            "cannot sync temporary self-update result for {}",
            destination.display()
        )
    })?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "cannot persist self-update result {}",
                destination.display()
            )
        })?;
    Ok(())
}

#[cfg(any(windows, test))]
fn now_unix_ms() -> Result<u64> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(milliseconds).context("current Unix timestamp does not fit in u64")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{StoredStatus, read, write_failure, write_scheduled, write_success};

    #[test]
    fn persists_and_replaces_self_update_results() {
        let directory = tempdir().unwrap();
        assert!(read(directory.path()).unwrap().is_none());

        write_scheduled(directory.path(), "1.2.3").unwrap();
        let scheduled = read(directory.path()).unwrap().unwrap();
        assert_eq!(scheduled.status, StoredStatus::Scheduled);

        write_success(directory.path(), "1.2.3").unwrap();
        let success = read(directory.path()).unwrap().unwrap();
        assert_eq!(success.status, StoredStatus::Success);
        assert_eq!(success.version, "1.2.3");
        assert!(success.message.is_none());

        write_failure(directory.path(), "1.2.4", "replacement failed".to_owned()).unwrap();
        let failed = read(directory.path()).unwrap().unwrap();
        assert_eq!(failed.status, StoredStatus::Failed);
        assert_eq!(failed.version, "1.2.4");
        assert_eq!(failed.message.as_deref(), Some("replacement failed"));
    }
}
