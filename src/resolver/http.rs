use anyhow::Result;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderName};
use sha2::{Digest, Sha256};

use crate::domain::{ArtifactConfig, ResolvedArtifact, ResolvedRelease, Tool};
use crate::error::UpdaterError;
use crate::paths::filename_from_url;

use super::util::{expected_sha256, incompatible_artifact};

pub(super) fn resolve(
    client: &Client,
    tool: &Tool,
    release_url: &str,
    version_headers: &[String],
) -> Result<ResolvedRelease> {
    let response = client
        .head(release_url)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("HTTP metadata request failed: {error}"),
        })?;
    let version = version_from_headers(response.headers(), version_headers).ok_or_else(|| {
        UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("none of the version headers were present: {version_headers:?}"),
        }
    })?;
    let mut artifacts = Vec::new();
    for artifact in &tool.artifacts {
        let artifact = match artifact {
            ArtifactConfig::ReleaseUrl => ResolvedArtifact {
                url: release_url.to_owned(),
                filename: filename_from_url(release_url),
                expected_sha256: None,
            },
            ArtifactConfig::DirectUrl { url, .. } => ResolvedArtifact {
                url: url.clone(),
                filename: filename_from_url(url),
                expected_sha256: expected_sha256(artifact, &version),
            },
            ArtifactConfig::UrlTemplate { url, .. } => {
                let url = url.replace("{version}", &version);
                ResolvedArtifact {
                    filename: filename_from_url(&url),
                    url,
                    expected_sha256: expected_sha256(artifact, &version),
                }
            }
            ArtifactConfig::GithubAsset { .. } => {
                return Err(incompatible_artifact(tool, "github-asset").into());
            }
            ArtifactConfig::GithubAssets { .. } => {
                return Err(incompatible_artifact(tool, "github-assets").into());
            }
            ArtifactConfig::GithubSource { .. } => {
                return Err(incompatible_artifact(tool, "github-source").into());
            }
            ArtifactConfig::PageLink { .. } => {
                return Err(incompatible_artifact(tool, "page-link").into());
            }
        };
        artifacts.push(artifact);
    }
    Ok(ResolvedRelease { version, artifacts })
}

fn version_from_headers(headers: &HeaderMap, names: &[String]) -> Option<String> {
    for name in names {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Some(value) = headers.get(name) else {
            continue;
        };
        let mut digest = Sha256::new();
        digest.update(value.as_bytes());
        return Some(
            digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use reqwest::header::{ETAG, HeaderMap, HeaderValue};

    use super::version_from_headers;

    #[test]
    fn hashes_first_available_version_header() {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("abc"));
        let version = version_from_headers(&headers, &["etag".to_owned()]);
        assert_eq!(
            version.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }
}
