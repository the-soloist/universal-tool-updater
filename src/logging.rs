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
    let path = directory.join(log_filename()?);
    let file =
        File::create(&path).with_context(|| format!("cannot create run log {}", path.display()))?;

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
    Ok(path)
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
    use std::path::Path;

    use super::{display_name, log_directory};

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
}
