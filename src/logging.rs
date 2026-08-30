use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use universal_tool_updater::output::ProgressAwareMakeWriter;

pub(crate) fn init(verbose: bool, requested_directory: Option<&Path>) -> Result<PathBuf> {
    let directory = log_directory(requested_directory)?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("cannot create log directory {}", directory.display()))?;
    let (file, path) = create_run_log(&directory, &log_filename()?)?;

    let fallback = if verbose { "debug" } else { "info" };
    let terminal_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    let terminal = tracing_subscriber::fmt::layer()
        .without_time()
        .with_target(false)
        .compact()
        .with_writer(ProgressAwareMakeWriter)
        .with_filter(terminal_filter);
    let run_log = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .compact()
        .with_writer(file)
        .with_filter(EnvFilter::new("universal_tool_updater=debug,updater=debug"));

    tracing_subscriber::registry()
        .with(terminal)
        .with(run_log)
        .init();
    prune_old_logs(&directory, RETAINED_LOG_FILES);
    Ok(path)
}

/// Number of `updater-*.log` run logs kept in the log directory; older files
/// are deleted newest-mtime-first.
const RETAINED_LOG_FILES: usize = 30;

/// Renamed retries after the primary run log name is already taken.
const LOG_NAME_RETRIES: usize = 3;

/// Creates the run log with create_new semantics: a pre-existing file
/// (including a symlink planted at the expected name) cannot be truncated or
/// written through. Name collisions retry with an incrementing suffix.
fn create_run_log(directory: &Path, filename: &str) -> Result<(File, PathBuf)> {
    let stem = filename.strip_suffix(".log").unwrap_or(filename);
    let mut candidate = directory.join(filename);
    for attempt in 0..=LOG_NAME_RETRIES {
        match File::options()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                candidate = directory.join(format!("{stem}-{}.log", attempt + 1));
            }
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("cannot create run log {}", candidate.display())));
            }
        }
    }
    Err(anyhow::anyhow!(
        "cannot create run log: {} and its {} alternate names already exist",
        directory.join(filename).display(),
        LOG_NAME_RETRIES
    ))
}

fn prune_old_logs(directory: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let is_run_log = name.starts_with("updater-") && name.ends_with(".log");
            is_run_log.then(|| {
                let modified = entry.metadata().ok().and_then(|data| data.modified().ok());
                (modified, entry.path())
            })
        })
        .collect::<Vec<_>>();
    logs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in logs.into_iter().skip(keep) {
        if let Err(error) = fs::remove_file(&path) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot prune old run log"
            );
        }
    }
}

pub(crate) fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_owned())
}

fn log_directory(requested: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = requested {
        return if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(std::env::current_dir()
                .context("cannot determine the working directory")?
                .join(path))
        };
    }

    let executable = std::env::current_exe().context("cannot determine the updater path")?;
    let parent = executable.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "updater path {} has no parent directory",
            executable.display()
        )
    })?;
    Ok(parent.join("logs"))
}

fn log_filename() -> Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is earlier than the Unix epoch")?
        .as_millis();
    Ok(format!("updater-{timestamp}-{}.log", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, FileTimes};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use tempfile::tempdir;

    use super::{create_run_log, display_name, log_directory, prune_old_logs};

    #[test]
    fn displays_only_the_path_filename() {
        assert_eq!(
            display_name(Path::new("/tmp/updater/updater-run.log")),
            "updater-run.log"
        );
    }

    #[test]
    fn resolves_relative_log_directories_from_the_working_directory() {
        assert_eq!(
            log_directory(Some(Path::new("run-logs"))).unwrap(),
            std::env::current_dir().unwrap().join("run-logs")
        );
    }

    #[test]
    fn preserves_absolute_log_directories() {
        let path = std::env::temp_dir().join("updater-run-logs");
        assert_eq!(log_directory(Some(&path)).unwrap(), path);
    }

    #[test]
    fn retries_with_a_suffixed_name_when_the_log_file_already_exists() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("updater-100-9.log"), "pre-existing").unwrap();

        let (file, path) = create_run_log(directory.path(), "updater-100-9.log").unwrap();
        assert_eq!(path, directory.path().join("updater-100-9-1.log"));
        drop(file);
        assert_eq!(
            fs::read_to_string(directory.path().join("updater-100-9.log")).unwrap(),
            "pre-existing",
            "the pre-existing log must not be truncated"
        );
    }

    #[test]
    fn gives_up_after_the_configured_number_of_retries() {
        let directory = tempdir().unwrap();
        for suffix in ["", "-1", "-2", "-3"] {
            fs::write(
                directory.path().join(format!("updater-7-1{suffix}.log")),
                "pre-existing",
            )
            .unwrap();
        }

        assert!(create_run_log(directory.path(), "updater-7-1.log").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_pre_existing_symlink() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("target.log");
        fs::write(&target, "elsewhere").unwrap();
        std::os::unix::fs::symlink(&target, directory.path().join("updater-5-5.log")).unwrap();

        let (_, path) = create_run_log(directory.path(), "updater-5-5.log").unwrap();

        assert_eq!(path, directory.path().join("updater-5-5-1.log"));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "elsewhere",
            "create_new must not write through the planted symlink"
        );
    }

    #[test]
    fn retains_only_the_newest_run_logs() {
        let directory = tempdir().unwrap();
        let base = SystemTime::now() - Duration::from_secs(600);
        for index in 0..5 {
            let path = directory.path().join(format!("updater-{index:04}-0.log"));
            File::create(&path).unwrap();
            let modified = base + Duration::from_secs(index * 60);
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(modified))
                .unwrap();
        }
        fs::write(directory.path().join("updater-notes.txt"), "unrelated").unwrap();
        fs::write(directory.path().join("other.log"), "unrelated").unwrap();

        prune_old_logs(directory.path(), 2);

        let mut remaining = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        remaining.sort();
        assert_eq!(
            remaining,
            vec![
                "other.log".to_owned(),
                "updater-0003-0.log".to_owned(),
                "updater-0004-0.log".to_owned(),
                "updater-notes.txt".to_owned(),
            ]
        );
    }
}
