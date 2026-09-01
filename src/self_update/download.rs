use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use console::Term;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};

const TRANSFER_ATTEMPTS: usize = 3;

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
    tracing::info!(url, filename, path = %destination.display(), "self-update download started");
    println!("Downloading {filename}");
    for attempt in 1..=TRANSFER_ATTEMPTS {
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(error) if attempt < TRANSFER_ATTEMPTS => {
                tracing::warn!(attempt, error = %error, "self-update request failed; retrying");
                thread::sleep(Duration::from_millis(500 * attempt as u64));
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot download {filename} from {url}"));
            }
        };
        let status = response.status();
        if !status.is_success() {
            if attempt < TRANSFER_ATTEMPTS && retryable_status(status) {
                tracing::warn!(attempt, %status, "self-update server error; retrying");
                thread::sleep(Duration::from_millis(500 * attempt as u64));
                continue;
            }
            bail!("cannot download {filename} from {url}: HTTP status {status}");
        }
        match transfer_response(response, destination, filename)? {
            TransferOutcome::Complete(bytes) => {
                tracing::info!(url, filename, bytes, path = %destination.display(), "self-update download completed");
                return Ok(());
            }
            TransferOutcome::Interrupted(message) if attempt < TRANSFER_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    error = message,
                    "self-update response body failed; retrying"
                );
                thread::sleep(Duration::from_millis(500 * attempt as u64));
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
    let mut output = File::create(destination)
        .with_context(|| format!("cannot create download file {}", destination.display()))?;
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                output.write_all(&buffer[..read]).with_context(|| {
                    format!("cannot write download file {}", destination.display())
                })?;
                downloaded += read as u64;
                progress.inc(read as u64);
            }
            Err(error) => {
                progress.finish_and_clear();
                return Ok(TransferOutcome::Interrupted(format!(
                    "cannot read response body: {error}"
                )));
            }
        }
    }
    output
        .sync_all()
        .with_context(|| format!("cannot sync download file {}", destination.display()))?;
    progress.finish_and_clear();
    if let Some(total) = total
        && total != downloaded
    {
        return Ok(TransferOutcome::Interrupted(format!(
            "response ended at {downloaded} bytes, expected {total}"
        )));
    }
    Ok(TransferOutcome::Complete(downloaded))
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

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}
