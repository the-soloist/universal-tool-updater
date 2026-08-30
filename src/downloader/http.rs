use std::thread;

use anyhow::Result;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, HeaderName, IF_RANGE, LAST_MODIFIED, RANGE,
};

use crate::domain::Tool;
use crate::error::UpdaterError;
use crate::paths::{decode_url_component, safe_filename};

use super::transfer::{ATTEMPTS, backoff_delay, is_retryable_status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ByteRange {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) total: Option<u64>,
}

impl ByteRange {
    pub(super) fn response_length(self) -> Option<u64> {
        self.end.checked_sub(self.start)?.checked_add(1)
    }

    pub(super) fn end_offset(self) -> Option<u64> {
        self.end.checked_add(1)
    }
}

pub(super) fn send_with_retry(
    client: &Client,
    tool: &Tool,
    url: &str,
    offset: u64,
    validator: Option<&str>,
) -> Result<Response> {
    for attempt in 1..=ATTEMPTS {
        let mut request = client.get(url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
            if let Some(validator) = validator {
                request = request.header(IF_RANGE, validator);
            }
        }
        let response = request.send().and_then(|response| {
            if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                Ok(response)
            } else {
                response.error_for_status()
            }
        });
        match response {
            Ok(response) => return Ok(response),
            Err(error) => {
                let retry = attempt < ATTEMPTS && is_retryable(&error);
                if retry {
                    tracing::warn!(
                        tool = %tool.id,
                        attempt,
                        attempts = ATTEMPTS,
                        error = %error,
                        "download request failed; retrying"
                    );
                    thread::sleep(backoff_delay(attempt));
                    continue;
                }

                let detail = format!("{:#}", anyhow::Error::new(error));
                return Err(UpdaterError::Download {
                    tool: tool.id.clone(),
                    message: format!(
                        "request to {url} failed after {attempt} attempt(s): {detail}"
                    ),
                }
                .into());
            }
        }
    }

    unreachable!("download retry loop always returns")
}

pub(super) fn response_header(response: &Response, name: &HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

pub(super) fn validator_unchanged(response: &Response, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let actual = [ETAG, LAST_MODIFIED]
        .iter()
        .filter_map(|name| response_header(response, name))
        .collect::<Vec<_>>();
    actual.iter().any(|value| value == expected)
}

pub(super) fn byte_range(response: &Response) -> Option<ByteRange> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    if end < start {
        return None;
    }
    let total = (total != "*").then(|| total.parse().ok()).flatten();
    Some(ByteRange { start, end, total })
}

pub(super) fn unsatisfied_total(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes */")?
        .parse()
        .ok()
}

pub(super) fn filename_from_disposition(response: &Response) -> Option<String> {
    let value = response.headers().get(CONTENT_DISPOSITION)?.to_str().ok()?;
    filename_from_content_disposition(value)
}

fn filename_from_content_disposition(value: &str) -> Option<String> {
    let parameters = value.split(';').skip(1).filter_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        Some((name.trim(), value.trim().trim_matches(['"', '\''])))
    });
    let mut fallback = None;
    for (name, value) in parameters {
        if name.eq_ignore_ascii_case("filename*") {
            let Some((charset, encoded)) = value.split_once("''") else {
                continue;
            };
            if !charset.eq_ignore_ascii_case("utf-8") {
                continue;
            }
            if let Some(filename) =
                decode_url_component(encoded).and_then(|value| safe_filename(&value))
            {
                return Some(filename);
            }
        } else if name.eq_ignore_ascii_case("filename") && fallback.is_none() {
            fallback = safe_filename(value);
        }
    }
    fallback
}

fn is_retryable(error: &reqwest::Error) -> bool {
    is_retryable_status(error.status())
}

#[cfg(test)]
mod tests {
    use super::filename_from_content_disposition;

    #[test]
    fn parses_standard_and_utf8_content_disposition_filenames() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=tool.zip").as_deref(),
            Some("tool.zip")
        );
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename=fallback.zip; filename*=UTF-8''%E5%B7%A5%E5%85%B7.zip"
            )
            .as_deref(),
            Some("工具.zip")
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename=CON.txt"),
            None
        );
    }
}
