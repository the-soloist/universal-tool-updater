use anyhow::Result;
use chardetng::EncodingDetector;
use reqwest::blocking::{Client, Response};
use url::Url;

use crate::domain::Tool;
use crate::error::UpdaterError;
use crate::paths::safe_filename;

pub(super) fn get_text(client: &Client, tool: &Tool, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("request to {url} failed: {error}"),
        })?;
    let bytes = response.bytes().map_err(|error| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("cannot read response from {url}: {error}"),
    })?;
    Ok(decode(&bytes))
}

pub(super) fn decode(bytes: &[u8]) -> String {
    let mut detector = EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, true);
    let (decoded, _, _) = encoding.decode(bytes);
    decoded.into_owned()
}

pub(super) fn filename_from_url(value: &str) -> Option<String> {
    Url::parse(value).ok().and_then(|url| {
        url.path_segments()
            .and_then(|mut parts| parts.next_back())
            .and_then(safe_filename)
    })
}

pub(super) fn incompatible_artifact(tool: &Tool, artifact: &str) -> UpdaterError {
    UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("artifact type {artifact} is incompatible with this release provider"),
    }
}
