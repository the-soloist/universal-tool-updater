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
    allow_insecure_transports: bool,
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
            validate_scraped_url(tool, &resolved_url, allow_insecure_transports)?;
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

/// 运行时对页面抓取 URL 的复验，镜像配置期规则：默认仅 HTTPS，
/// opt-in 后允许明文 HTTP，且必须带主机名。
fn validate_scraped_url(
    tool: &Tool,
    value: &str,
    allow_insecure_transports: bool,
) -> std::result::Result<(), UpdaterError> {
    let rejected = |reason: String| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("scraped download URL {value:?} rejected: {reason}"),
    };
    let parsed = Url::parse(value).map_err(|error| rejected(format!("invalid URL: {error}")))?;
    let scheme_allowed =
        parsed.scheme() == "https" || (allow_insecure_transports && parsed.scheme() == "http");
    if !scheme_allowed || parsed.host_str().is_none_or(str::is_empty) {
        let allowed = if allow_insecure_transports {
            "HTTP or HTTPS"
        } else {
            "HTTPS"
        };
        return Err(rejected(format!(
            "URL must use {allowed} and include a host"
        )));
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
    fn rejects_scraped_page_links_that_are_not_https() {
        let tool = test_tool("demo", "/toolkit/demo");

        let error = validate_scraped_url(&tool, "http://example.com/tool.zip", false).unwrap_err();
        assert!(
            error.to_string().contains("http://example.com/tool.zip"),
            "expected the rejected URL in the error, got {error}"
        );
        assert!(
            error.to_string().contains("HTTPS"),
            "expected the scheme reason, got {error}"
        );
    }

    #[test]
    fn allows_plain_http_scraped_page_links_only_when_opted_in() {
        let tool = test_tool("demo", "/toolkit/demo");

        assert!(validate_scraped_url(&tool, "http://example.com/tool.zip", true).is_ok());
        assert!(validate_scraped_url(&tool, "https://example.com/tool.zip", false).is_ok());
    }

    #[test]
    fn rejects_scraped_page_links_without_a_host() {
        let tool = test_tool("demo", "/toolkit/demo");

        for value in ["file:///tmp/tool.zip", "javascript:alert(1)"] {
            let error = validate_scraped_url(&tool, value, false).unwrap_err();
            assert!(
                error.to_string().contains("include a host"),
                "expected a host rejection for {value}, got {error}"
            );
        }
    }
}
