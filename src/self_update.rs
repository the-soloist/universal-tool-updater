mod checksum;
mod github;
mod replacement;
mod status;

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::Term;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::{Client, Response};
use semver::Version;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::archive::ArchiveService;
use crate::downloader::transfer::{self, TransferFailure};

use replacement::InstallOutcome;

const CHECKSUMS_ASSET: &str = "SHA256SUMS.txt";
const LOCK_FILENAME: &str = ".updater-self-update.lock";
const MAX_CHECKSUM_FILE_SIZE: u64 = 1024 * 1024;
const WORK_DIRECTORY_PREFIX: &str = ".updater-self-update-";

#[derive(Debug, Clone, Copy)]
pub struct SelfUpdateOptions {
    pub check_only: bool,
    pub force: bool,
    pub status_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfUpdateOutcome {
    Completed,
    Scheduled,
}

pub fn run(options: SelfUpdateOptions) -> Result<SelfUpdateOutcome> {
    if options.status_only {
        return report_status();
    }
    let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the compiled updater version is not valid semver")?;
    let client = http_client()?;
    let token = github_token();
    println!("Checking for updater releases...");
    let release = github::latest(&client, token.as_deref())?;
    println!("Current version: v{current_version}");
    println!("Latest version:  {}", release.tag);

    if release.version < current_version {
        println!("The installed updater is newer than the latest stable release.");
        return Ok(SelfUpdateOutcome::Completed);
    }
    if options.check_only {
        if release.version > current_version {
            println!("An updater release is available.");
        } else {
            println!("The updater is current.");
        }
        return Ok(SelfUpdateOutcome::Completed);
    }
    if release.version == current_version && !options.force {
        println!("The updater is already current.");
        return Ok(SelfUpdateOutcome::Completed);
    }

    let target = current_updater()?;
    let target_parent = target
        .parent()
        .context("the current updater has no parent directory")?;
    let mut lock = UpdateLock::acquire(target_parent)?;
    let work_dir = tempfile::Builder::new()
        .prefix(WORK_DIRECTORY_PREFIX)
        .tempdir_in(target_parent)
        .with_context(|| {
            format!(
                "cannot create a self-update workspace beside {}",
                target.display()
            )
        })?;

    let platform = asset_platform(std::env::consts::OS, std::env::consts::ARCH)?;
    let archive_name = format!("updater-{}-{platform}.7z", release.tag);
    let archive_url = release.asset_url(&archive_name)?;
    let checksums_url = release.asset_url(CHECKSUMS_ASSET)?;
    let checksums_path = work_dir.path().join(CHECKSUMS_ASSET);
    let archive_path = work_dir.path().join(&archive_name);

    download(&client, checksums_url, &checksums_path, CHECKSUMS_ASSET)?;
    let checksum_size = fs::metadata(&checksums_path)
        .with_context(|| format!("cannot inspect {}", checksums_path.display()))?
        .len();
    if checksum_size > MAX_CHECKSUM_FILE_SIZE {
        bail!("SHA256SUMS.txt exceeds the 1 MiB safety limit");
    }
    let checksums =
        fs::read_to_string(&checksums_path).context("SHA256SUMS.txt is not valid UTF-8")?;
    let expected = checksum::expected_sha256(&checksums, &archive_name)?;

    download(&client, archive_url, &archive_path, &archive_name)?;
    checksum::verify(&archive_path, &expected)?;
    println!("Verified SHA-256 for {archive_name}");

    let extract_dir = work_dir.path().join("extracted");
    ArchiveService::default().extract(&archive_path, &extract_dir, None)?;
    let candidate = find_candidate(&extract_dir)?;
    prepare_candidate(
        #[cfg(unix)]
        &candidate,
    )?;
    verify_candidate(&candidate, &release.version)?;

    match replacement::install(&target, &candidate, work_dir, &release.version, &mut lock)? {
        InstallOutcome::Completed => {
            println!("updater updated successfully to {}", release.tag);
            Ok(SelfUpdateOutcome::Completed)
        }
        #[cfg(windows)]
        InstallOutcome::Scheduled => {
            println!("The verified update will be installed after this process exits.");
            println!("Run `updater self-update --status` to inspect the helper result.");
            Ok(SelfUpdateOutcome::Scheduled)
        }
    }
}

fn report_status() -> Result<SelfUpdateOutcome> {
    let target = current_updater()?;
    let directory = target
        .parent()
        .context("the current updater has no parent directory")?;
    let Some(result) = status::read(directory)? else {
        bail!(
            "no asynchronous self-update result exists at {}",
            directory.join(status::RESULT_FILENAME).display()
        );
    };
    tracing::debug!(
        version = %result.version,
        updated_at_unix_ms = result.updated_at_unix_ms,
        "loaded persisted self-update result"
    );
    match result.status {
        status::StoredStatus::Scheduled => {
            println!("Self-update to v{} is scheduled.", result.version);
            if let Some(message) = result.message {
                println!("{message}");
            }
            Ok(SelfUpdateOutcome::Scheduled)
        }
        status::StoredStatus::Success => {
            println!("Self-update to v{} completed successfully.", result.version);
            Ok(SelfUpdateOutcome::Completed)
        }
        status::StoredStatus::Failed => {
            let message = result
                .message
                .unwrap_or_else(|| "the Windows replacement helper failed".to_owned());
            bail!("self-update to v{} failed: {message}", result.version);
        }
    }
}

#[cfg(windows)]
pub fn replace_helper(target: &Path, candidate: &Path, version: &str) -> Result<()> {
    replacement::replace_helper(target, candidate, version)
}

#[cfg(windows)]
pub fn cleanup_helper(work_dir: &Path) -> Result<()> {
    replacement::cleanup_helper(work_dir)
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!(
            "universal-tool-updater/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(10 * 60))
        .build()
        .context("cannot create the self-update HTTP client")
}

fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

fn asset_platform(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("windows"),
        ("linux", "x86_64") => Ok("linux"),
        ("macos", "x86_64" | "aarch64") => Ok("macos"),
        _ => bail!("self-update is unsupported on {os}/{arch}"),
    }
}

fn current_updater() -> Result<PathBuf> {
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

fn find_candidate(directory: &Path) -> Result<PathBuf> {
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

fn prepare_candidate(#[cfg(unix)] path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        fs::set_permissions(path, permissions).with_context(|| {
            format!(
                "cannot make candidate updater executable at {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn verify_candidate(path: &Path, expected: &Version) -> Result<()> {
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

fn download(client: &Client, url: &str, destination: &Path, filename: &str) -> Result<()> {
    tracing::info!(url = %transfer::redact_url(url), filename, path = %destination.display(), "self-update download started");
    println!("Downloading {filename}");
    for attempt in 1..=transfer::ATTEMPTS {
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(error) if attempt < transfer::ATTEMPTS => {
                tracing::warn!(attempt, error = %error, "self-update request failed; retrying");
                thread::sleep(transfer::backoff_delay(attempt));
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot download {filename} from {url}"));
            }
        };
        let status = response.status();
        if !status.is_success() {
            if attempt < transfer::ATTEMPTS && transfer::is_retryable_status(Some(status)) {
                tracing::warn!(attempt, %status, "self-update server error; retrying");
                thread::sleep(transfer::backoff_delay(attempt));
                continue;
            }
            bail!("cannot download {filename} from {url}: HTTP status {status}");
        }
        match transfer_response(response, destination, filename)? {
            TransferOutcome::Complete(bytes) => {
                tracing::info!(url = %transfer::redact_url(url), filename, bytes, path = %destination.display(), "self-update download completed");
                return Ok(());
            }
            TransferOutcome::Interrupted(message) if attempt < transfer::ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    error = message,
                    "self-update response body failed; retrying"
                );
                thread::sleep(transfer::backoff_delay(attempt));
            }
            TransferOutcome::Interrupted(message) => {
                bail!(
                    "download of {filename} from {url} failed after {attempt} attempt(s): {message}"
                );
            }
        }
    }
    unreachable!("self-update transfer loop always returns")
}

enum TransferOutcome {
    Complete(u64),
    Interrupted(String),
}

fn transfer_response(
    mut response: Response,
    destination: &Path,
    filename: &str,
) -> Result<TransferOutcome> {
    let total = response.content_length();
    let progress = download_progress(filename, total);
    let max_bytes = crate::archive::Limits::default().max_total_bytes;
    let mut output = File::create(destination)
        .with_context(|| format!("cannot create download file {}", destination.display()))?;
    // Self-update verifies against SHA256SUMS.txt after the transfer, so
    // the pipelined digest is computed and discarded here.
    let mut digest = Sha256::new();
    let transferred = transfer::stream_response(
        &mut response,
        &mut output,
        destination,
        0,
        total,
        max_bytes,
        |chunk| {
            digest.update(chunk);
            progress.inc(chunk.len() as u64);
        },
    );
    drop(digest);
    progress.finish_and_clear();
    match transferred {
        Ok(bytes) => Ok(TransferOutcome::Complete(bytes)),
        Err(TransferFailure::Fatal(error)) => Err(error),
        Err(TransferFailure::Retryable { message, .. }) => {
            Ok(TransferOutcome::Interrupted(message))
        }
        Err(TransferFailure::LimitExceeded { written }) => bail!(
            "download of {filename} wrote {written} bytes, exceeding the transfer limit of {max_bytes} bytes"
        ),
    }
}

fn download_progress(filename: &str, total: Option<u64>) -> ProgressBar {
    if !Term::stderr().is_term() {
        return ProgressBar::hidden();
    }
    let progress = total
        .map(ProgressBar::new)
        .unwrap_or_else(ProgressBar::new_spinner);
    let style = if total.is_some() {
        ProgressStyle::with_template(
            "  {msg:36!} [{bar:28.green/black}] {bytes}/{total_bytes} {eta}",
        )
        .expect("static self-update progress template")
        .progress_chars("=>-")
    } else {
        ProgressStyle::with_template("  {spinner:.green} {msg} {bytes}")
            .expect("static self-update spinner template")
    };
    progress.set_style(style);
    progress.set_message(filename.to_owned());
    progress
}

struct UpdateLock {
    _file: File,
    #[cfg(any(windows, test))]
    path: PathBuf,
}

impl UpdateLock {
    fn acquire(directory: &Path) -> Result<Self> {
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
    fn acquire_for_helper(directory: &Path) -> Result<Self> {
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
    fn release_for_handoff(&self) -> Result<()> {
        self._file
            .unlock()
            .with_context(|| format!("cannot hand off self-update lock {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{LOCK_FILENAME, UpdateLock, asset_platform, find_candidate};

    #[test]
    fn maps_only_published_platforms() {
        assert_eq!(asset_platform("linux", "x86_64").unwrap(), "linux");
        assert_eq!(asset_platform("windows", "x86_64").unwrap(), "windows");
        assert_eq!(asset_platform("macos", "aarch64").unwrap(), "macos");
        assert_eq!(asset_platform("macos", "x86_64").unwrap(), "macos");
        assert!(asset_platform("linux", "aarch64").is_err());
    }

    #[test]
    fn requires_one_exact_candidate_file() {
        let directory = tempdir().unwrap();
        let expected = if cfg!(windows) {
            "updater.exe"
        } else {
            "updater"
        };
        let candidate = directory.path().join(expected);
        fs::write(&candidate, "binary").unwrap();
        assert_eq!(find_candidate(directory.path()).unwrap(), candidate);
        fs::write(directory.path().join("unexpected"), "extra").unwrap();
        assert!(find_candidate(directory.path()).is_err());
    }

    #[test]
    fn prevents_concurrent_self_updates() {
        let directory = tempdir().unwrap();
        let first = UpdateLock::acquire(directory.path()).unwrap();
        assert!(UpdateLock::acquire(directory.path()).is_err());
        drop(first);
        assert!(directory.path().join(LOCK_FILENAME).exists());
        UpdateLock::acquire(directory.path()).unwrap();
    }

    #[test]
    fn hands_the_process_lock_to_the_replacement_helper() {
        let directory = tempdir().unwrap();
        let first = UpdateLock::acquire(directory.path()).unwrap();
        first.release_for_handoff().unwrap();
        let helper = UpdateLock::acquire_for_helper(directory.path()).unwrap();
        assert_eq!(helper.path, directory.path().join(LOCK_FILENAME));
    }
}
