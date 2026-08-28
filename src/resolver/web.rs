use anyhow::Result;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use url::Url;

use crate::domain::{ArtifactConfig, ResolvedArtifact, ResolvedRelease, Tool};
use crate::error::UpdaterError;
use crate::paths::filename_from_url;

use super::util::{decode, incompatible_artifact};

pub(super) fn resolve(
    client: &Client,
    tool: &Tool,
    page_url: &str,
    version_pattern: &str,
    ignored: &[String],
) -> Result<ResolvedRelease> {
    let response = client
        .get(page_url)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("web request failed: {error}"),
        })?;
    let bytes = response.bytes().map_err(|error| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("cannot read web response: {error}"),
    })?;
    let body = decode(&bytes);
    let regex = Regex::new(version_pattern).map_err(|error| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("invalid version regex {version_pattern:?}: {error}"),
    })?;
    let version = regex
        .captures_iter(&body)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned()))
        .find(|version| !ignored.contains(version))
        .ok_or_else(|| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("version regex did not match {version_pattern:?}"),
        })?;

    let page = Url::parse(page_url).map_err(|error| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("invalid release URL {page_url:?}: {error}"),
    })?;
    let mut artifacts = Vec::new();
    for artifact in &tool.artifacts {
        let resolved_url = match artifact {
            ArtifactConfig::PageLink { pattern, base_url } => {
                let link_regex = Regex::new(pattern).map_err(|error| UpdaterError::Resolution {
                    tool: tool.id.clone(),
                    message: format!("invalid download link regex {pattern:?}: {error}"),
                })?;
                let capture =
                    link_regex
                        .captures(&body)
                        .ok_or_else(|| UpdaterError::Resolution {
                            tool: tool.id.clone(),
                            message: format!("download link regex did not match {pattern:?}"),
                        })?;
                let value = capture
                    .get(1)
                    .or_else(|| capture.get(0))
                    .ok_or_else(|| UpdaterError::Resolution {
                        tool: tool.id.clone(),
                        message: format!("download link regex produced no capture: {pattern:?}"),
                    })?
                    .as_str();
                if let Some(base) = base_url {
                    format!("{base}{value}")
                } else {
                    page.join(value)
                        .map_err(|error| UpdaterError::Resolution {
                            tool: tool.id.clone(),
                            message: format!("invalid relative download URL {value:?}: {error}"),
                        })?
                        .to_string()
                }
            }
            ArtifactConfig::DirectUrl { url } => url.clone(),
            ArtifactConfig::UrlTemplate { url } => url.replace("{version}", &version),
            ArtifactConfig::GithubAsset { .. } => {
                return Err(incompatible_artifact(tool, "github-asset").into());
            }
            ArtifactConfig::GithubAssets { .. } => {
                return Err(incompatible_artifact(tool, "github-assets").into());
            }
            ArtifactConfig::GithubSource { .. } => {
                return Err(incompatible_artifact(tool, "github-source").into());
            }
            ArtifactConfig::ReleaseUrl => {
                return Err(incompatible_artifact(tool, "release-url").into());
            }
        };
        artifacts.push(ResolvedArtifact {
            filename: filename_from_url(&resolved_url),
            url: resolved_url,
        });
    }
    Ok(ResolvedRelease { version, artifacts })
}
