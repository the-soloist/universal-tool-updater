mod candidate;
mod checksum;
mod download;
mod github;
mod lock;
mod replacement;
mod status;

#[cfg(test)]
mod tests;

use std::fs;
#[cfg(windows)]
use std::path::Path;

use anyhow::{Context, Result, bail};
use semver::Version;

use crate::archive::ArchiveService;

use candidate::{current_updater, find_candidate, prepare_candidate, verify_candidate};
use download::{download, github_token, http_client};
use lock::UpdateLock;
use replacement::InstallOutcome;

const CHECKSUMS_ASSET: &str = "SHA256SUMS.txt";
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
    ArchiveService.extract(&archive_path, &extract_dir, None)?;
    let candidate = find_candidate(&extract_dir)?;
    prepare_candidate(&candidate)?;
    verify_candidate(&candidate, &release.version)?;

    match replacement::install(&target, &candidate, work_dir, &release.version, &mut lock)? {
        #[cfg(unix)]
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

fn asset_platform(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("windows"),
        ("windows", "aarch64") => Ok("windows-arm64"),
        ("linux", "x86_64") => Ok("linux"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        _ => bail!("self-update is unsupported on {os}/{arch}"),
    }
}
