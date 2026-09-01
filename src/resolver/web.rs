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
                let value = normalize_page_link(value);
                if let Some(base) = base_url {
                    format!("{base}{value}")
                } else {
                    page.join(&value)
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
        let filename = if matches!(artifact, ArtifactConfig::PageLink { .. }) {
            None
        } else {
            filename_from_url(&resolved_url)
        };
        if matches!(artifact, ArtifactConfig::PageLink { .. }) {
            validate_scraped_url(tool, &resolved_url)?;
        }
        artifacts.push(ResolvedArtifact {
            filename,
            url: resolved_url,
        });
    }
    Ok(ResolvedRelease { version, artifacts })
}

fn normalize_page_link(value: &str) -> String {
    value.replace("\\/", "/").replace("&amp;", "&")
}

fn validate_scraped_url(tool: &Tool, value: &str) -> std::result::Result<(), UpdaterError> {
    let rejected = |reason: String| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("scraped download URL {value:?} rejected: {reason}"),
    };
    let parsed = Url::parse(value).map_err(|error| rejected(format!("invalid URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(rejected(
            "URL must use HTTP or HTTPS and include a host".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::test_support::tool as test_tool;

    use super::{normalize_page_link, validate_scraped_url};

    #[test]
    fn normalizes_json_and_html_escaped_page_links() {
        assert_eq!(
            normalize_page_link(
                r"https:\/\/gobies.org\/download\/release?type=full_url&amp;id=355"
            ),
            "https://gobies.org/download/release?type=full_url&id=355"
        );
    }

    #[test]
    fn accepts_only_http_page_links_with_a_host() {
        let tool = test_tool("demo", "/toolkit/demo");

        assert!(validate_scraped_url(&tool, "https://example.com/tool.zip").is_ok());
        assert!(validate_scraped_url(&tool, "http://example.com/tool.zip").is_ok());
        for value in ["file:///tmp/tool.zip", "javascript:alert(1)"] {
            let error = validate_scraped_url(&tool, value).unwrap_err();
            assert!(error.to_string().contains("include a host"));
        }
    }
}
