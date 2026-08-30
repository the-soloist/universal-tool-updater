use std::fs::File;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::Term;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::{Client, Response};
use sha2::{Digest, Sha256};

use crate::downloader::transfer::{self, TransferFailure};

pub(super) fn http_client() -> Result<Client> {
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

pub(super) fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

pub(super) fn download(
    client: &Client,
    url: &str,
    destination: &Path,
    filename: &str,
) -> Result<()> {
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
        |chunk, _output| {
            digest.update(chunk);
            progress.inc(chunk.len() as u64);
            Ok(())
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
