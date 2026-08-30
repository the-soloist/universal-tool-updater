use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::StatusCode;

pub(crate) const ATTEMPTS: usize = 3;
pub(crate) const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Transfer failures split by recovery strategy: fatal failures abort the
/// download, retryable failures consume one retry slot and carry the number of
/// body bytes already written so callers can resume from the right offset,
/// limit failures mean the body streamed past the configured byte ceiling.
pub(crate) enum TransferFailure {
    Fatal(anyhow::Error),
    Retryable { written: u64, message: String },
    LimitExceeded { written: u64 },
}

pub(crate) fn is_retryable_status(status: Option<StatusCode>) -> bool {
    status.is_none_or(|status| {
        status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
    })
}

pub(crate) fn backoff_delay(attempt: usize) -> Duration {
    RETRY_DELAY * attempt as u32
}

/// Collapses the URL query string so credentials embedded in the query never
/// reach the log.
pub(crate) fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((prefix, _)) => format!("{prefix}?..."),
        None => url.to_owned(),
    }
}

/// Streams a response body into `output` in 64 KiB chunks, syncs the file and
/// verifies the expected end offset. `start` is the byte offset already stored
/// in `output` so resumed transfers keep counting from the right position.
/// `max_bytes` bounds the total byte count regardless of any declared
/// Content-Length, so unannounced or chunked bodies cannot stream past the
/// configured ceiling. `on_chunk` observes every chunk that was written along
/// with the output handle, letting callers hash the body in lockstep with the
/// bytes on disk and persist periodic checkpoints; a failing callback aborts
/// the transfer as a fatal error.
pub(crate) fn stream_response(
    response: &mut impl Read,
    output: &mut fs::File,
    destination: &Path,
    start: u64,
    expected_end: Option<u64>,
    max_bytes: u64,
    mut on_chunk: impl FnMut(&[u8], &mut fs::File) -> Result<(), anyhow::Error>,
) -> Result<u64, TransferFailure> {
    let mut downloaded = start;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if let Err(error) = output.write_all(&buffer[..read]) {
                    return Err(TransferFailure::Fatal(anyhow!(error).context(format!(
                        "cannot write download file {}",
                        destination.display()
                    ))));
                }
                downloaded += read as u64;
                if downloaded > max_bytes {
                    return Err(TransferFailure::LimitExceeded {
                        written: downloaded,
                    });
                }
                if let Err(error) = on_chunk(&buffer[..read], output) {
                    return Err(TransferFailure::Fatal(error));
                }
            }
            Err(error) => {
                return Err(TransferFailure::Retryable {
                    written: downloaded,
                    message: format!("cannot read response body: {error}"),
                });
            }
        }
    }
    if let Err(error) = output.sync_all() {
        return Err(TransferFailure::Fatal(anyhow!(error).context(format!(
            "cannot sync download file {}",
            destination.display()
        ))));
    }
    if let Some(expected) = expected_end
        && downloaded != expected
    {
        return Err(TransferFailure::Retryable {
            written: downloaded,
            message: format!("download response ended at {downloaded}, expected {expected} bytes"),
        });
    }
    Ok(downloaded)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{redact_url, stream_response};

    #[test]
    fn truncates_only_the_query_segment() {
        assert_eq!(
            redact_url("https://example.com/a.zip"),
            "https://example.com/a.zip"
        );
        assert_eq!(
            redact_url("https://example.com/a.zip?token=secret&page=2"),
            "https://example.com/a.zip?..."
        );
    }

    /// Golden equivalence for the pipelined digest: the hasher fed inside
    /// stream_response must equal a full re-read of the written file.
    #[test]
    fn streamed_digest_matches_a_full_re_read_of_the_output() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let body: Vec<u8> = (0..300 * 1024)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 32) as u8
            })
            .collect();
        let mut output = fs::File::create(&path).unwrap();
        let mut reader: &[u8] = &body;
        let mut hasher = Sha256::new();
        let transferred = stream_response(
            &mut reader,
            &mut output,
            &path,
            0,
            Some(body.len() as u64),
            body.len() as u64 + 1,
            |chunk, _output| {
                hasher.update(chunk);
                Ok(())
            },
        );
        drop(output);
        let written = transferred
            .map_err(|failure| match failure {
                super::TransferFailure::Fatal(error) => format!("fatal: {error:#}"),
                super::TransferFailure::Retryable { message, .. } => message,
                super::TransferFailure::LimitExceeded { written } => {
                    format!("limit exceeded at {written} bytes")
                }
            })
            .unwrap();

        assert_eq!(written, body.len() as u64);
        let streamed = hasher.finalize();
        let mut re_read = Sha256::new();
        let mut file = fs::File::open(&path).unwrap();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            re_read.update(&buffer[..read]);
        }
        assert_eq!(
            streamed,
            re_read.finalize(),
            "the streamed digest must equal the re-read digest"
        );
    }
}
