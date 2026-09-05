use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is earlier than the Unix epoch")?;
    let timestamp = format_timestamp(elapsed)?;
    Ok(format!("updater-{timestamp}-{}.log", std::process::id()))
}

/// Formats a UTC timestamp without path separators or characters forbidden in
/// Windows filenames. The resulting form remains lexicographically sortable.
fn format_timestamp(elapsed: Duration) -> Result<String> {
    let total_seconds = elapsed.as_secs();
    let days = i64::try_from(total_seconds / 86_400)
        .context("Unix timestamp is too far in the future to format")?;
    let seconds_of_day = total_seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let milliseconds = elapsed.subsec_millis();
    let (year, month, day) = civil_from_days(days);

    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}.{milliseconds:03}Z"
    ))
}

// Converts days since 1970-01-01 to a Gregorian date (UTC).
// This is the civil date algorithm from Howard Hinnant's public-domain work.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use std::time::Duration;

    use super::{civil_from_days, display_name, format_timestamp, log_directory};

    #[test]
    fn formats_log_timestamp_as_a_sortable_utc_filename_component() {
        assert_eq!(
            format_timestamp(Duration::from_millis(0)).unwrap(),
            "1970-01-01T00-00-00.000Z"
        );
        assert_eq!(
            format_timestamp(Duration::from_millis(1_704_067_200_123)).unwrap(),
            "2024-01-01T00-00-00.123Z"
        );
    }

    #[test]
    fn converts_leap_day_dates_correctly() {
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
    }

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
