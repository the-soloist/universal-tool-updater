use anyhow::Result;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Deserialize;
use url::Url;

use crate::domain::{ArtifactConfig, ResolvedArtifact, ResolvedRelease, Tool};
use crate::error::UpdaterError;
use crate::paths::{filename_from_url, safe_filename};

use super::util::{get_text, incompatible_artifact};

type HtmlAssets = Vec<(String, String)>;

pub(super) fn resolve(
    client: &Client,
    token: Option<&str>,
    tool: &Tool,
    repository: &str,
    ignored: &[String],
    allow_prereleases: bool,
) -> Result<ResolvedRelease> {
    let (version, api_assets, mut html_assets) = if let Some(token) = token {
        let (version, assets) =
            release_from_api(client, token, tool, repository, ignored, allow_prereleases)?;
        (version, assets, None)
    } else {
        let (version, assets) =
            version_from_atom(client, tool, repository, ignored, allow_prereleases)?;
        (version, None, assets)
    };

    let mut artifacts = Vec::new();
    for artifact in &tool.artifacts {
        match artifact {
            ArtifactConfig::GithubAsset { pattern } => {
                let regex = asset_regex(tool, pattern)?;
                let resolved = matching_github_assets(
                    client,
                    tool,
                    repository,
                    &version,
                    &api_assets,
                    &mut html_assets,
                    &regex,
                )?
                .into_iter()
                .next()
                .ok_or_else(|| UpdaterError::Resolution {
                    tool: tool.id.clone(),
                    message: format!("no GitHub asset matched {pattern:?}"),
                })?;
                artifacts.push(resolved);
            }
            ArtifactConfig::GithubAssets { pattern } => {
                let regex = asset_regex(tool, pattern)?;
                let resolved = matching_github_assets(
                    client,
                    tool,
                    repository,
                    &version,
                    &api_assets,
                    &mut html_assets,
                    &regex,
                )?;
                if resolved.is_empty() {
                    return Err(UpdaterError::Resolution {
                        tool: tool.id.clone(),
                        message: format!("no GitHub assets matched {pattern:?}"),
                    }
                    .into());
                }
                artifacts.extend(resolved);
            }
            ArtifactConfig::GithubSource { format } => {
                let url =
                    format!("https://github.com/{repository}/archive/refs/tags/{version}.{format}");
                artifacts.push(ResolvedArtifact {
                    filename: filename_from_url(&url),
                    url,
                });
            }
            ArtifactConfig::DirectUrl { url } => artifacts.push(ResolvedArtifact {
                filename: filename_from_url(url),
                url: url.clone(),
            }),
            ArtifactConfig::UrlTemplate { url } => {
                let url = url.replace("{version}", &version);
                artifacts.push(ResolvedArtifact {
                    filename: filename_from_url(&url),
                    url,
                });
            }
            ArtifactConfig::PageLink { .. } => {
                return Err(incompatible_artifact(tool, "page-link").into());
            }
            ArtifactConfig::ReleaseUrl => {
                return Err(incompatible_artifact(tool, "release-url").into());
            }
        }
    }
    Ok(ResolvedRelease { version, artifacts })
}

fn asset_regex(tool: &Tool, pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|error| {
        UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("invalid asset regex {pattern:?}: {error}"),
        }
        .into()
    })
}

fn matching_github_assets(
    client: &Client,
    tool: &Tool,
    repository: &str,
    version: &str,
    api_assets: &Option<Vec<GithubAsset>>,
    html_assets: &mut Option<HtmlAssets>,
    regex: &Regex,
) -> Result<Vec<ResolvedArtifact>> {
    if let Some(assets) = api_assets {
        return Ok(matching_artifacts(
            regex,
            assets
                .iter()
                .map(|asset| (asset.name.as_str(), asset.browser_download_url.as_str())),
        ));
    }
    if html_assets.is_none() {
        *html_assets = Some(assets_from_html(client, tool, repository, version)?);
    }
    Ok(matching_artifacts(
        regex,
        html_assets
            .as_ref()
            .expect("GitHub HTML assets were initialized")
            .iter()
            .map(|(name, url)| (name.as_str(), url.as_str())),
    ))
}

fn matching_artifacts<'a>(
    regex: &Regex,
    assets: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<ResolvedArtifact> {
    assets
        .into_iter()
        .filter(|(name, _)| regex.is_match(name))
        .map(|(name, url)| ResolvedArtifact {
            url: url.to_owned(),
            filename: Some(name.to_owned()),
        })
        .collect()
}

fn release_from_api(
    client: &Client,
    token: &str,
    tool: &Tool,
    repository: &str,
    ignored: &[String],
    allow_prereleases: bool,
) -> Result<(String, Option<Vec<GithubAsset>>)> {
    let url = format!("https://api.github.com/repos/{repository}/releases?per_page=30");
    let releases: Vec<GithubRelease> = client
        .get(&url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("GitHub API request failed: {error}"),
        })?
        .json()
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("invalid GitHub API response: {error}"),
        })?;
    let required = required_asset_patterns(tool)?;
    let release =
        eligible_release(releases, &required, ignored, allow_prereleases).ok_or_else(|| {
            UpdaterError::Resolution {
                tool: tool.id.clone(),
                message: "no eligible GitHub release matched the configured assets".to_owned(),
            }
        })?;
    Ok((release.tag_name, Some(release.assets)))
}

fn eligible_release(
    releases: Vec<GithubRelease>,
    required: &[Regex],
    ignored: &[String],
    allow_prereleases: bool,
) -> Option<GithubRelease> {
    releases.into_iter().find(|release| {
        !release.draft
            && (allow_prereleases || !release.prerelease)
            && !ignored.contains(&release.tag_name)
            && release_assets_match(
                required,
                release.assets.iter().map(|asset| asset.name.as_str()),
            )
    })
}

fn version_from_atom(
    client: &Client,
    tool: &Tool,
    repository: &str,
    ignored: &[String],
    allow_prereleases: bool,
) -> Result<(String, Option<HtmlAssets>)> {
    let url = format!("https://github.com/{repository}/releases.atom");
    let body = get_text(client, tool, &url)?;
    let regex = Regex::new(r#"/releases/tag/([^\"<]+)"#).expect("static regex");
    let versions = regex
        .captures_iter(&body)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned()))
        .filter(|version| eligible_atom_version(version, ignored, allow_prereleases));
    let required = required_asset_patterns(tool)?;
    for version in versions {
        if required.is_empty() {
            return Ok((version, None));
        }
        let assets = assets_from_html(client, tool, repository, &version)?;
        if release_assets_match(&required, assets.iter().map(|(name, _)| name.as_str())) {
            return Ok((version, Some(assets)));
        }
    }
    Err(UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: "no eligible GitHub release matched the configured assets".to_owned(),
    }
    .into())
}

fn eligible_atom_version(version: &str, ignored: &[String], allow_prereleases: bool) -> bool {
    if ignored.iter().any(|ignored| ignored == version) {
        return false;
    }
    allow_prereleases
        || semver::Version::parse(version.strip_prefix('v').unwrap_or(version))
            .map(|version| version.pre.is_empty())
            .unwrap_or(true)
}

fn required_asset_patterns(tool: &Tool) -> Result<Vec<Regex>> {
    tool.artifacts
        .iter()
        .filter_map(|artifact| match artifact {
            ArtifactConfig::GithubAsset { pattern } | ArtifactConfig::GithubAssets { pattern } => {
                Some(asset_regex(tool, pattern))
            }
            _ => None,
        })
        .collect()
}

fn release_assets_match<'a>(required: &[Regex], names: impl IntoIterator<Item = &'a str>) -> bool {
    let names = names.into_iter().collect::<Vec<_>>();
    required
        .iter()
        .all(|pattern| names.iter().any(|name| pattern.is_match(name)))
}

fn assets_from_html(
    client: &Client,
    tool: &Tool,
    repository: &str,
    version: &str,
) -> Result<HtmlAssets> {
    let url = format!("https://github.com/{repository}/releases/expanded_assets/{version}");
    let body = get_text(client, tool, &url)?;
    let href = Regex::new(r#"href="([^"]+)""#).expect("static regex");
    let base = Url::parse("https://github.com").expect("static URL");
    let mut result = Vec::new();
    for capture in href.captures_iter(&body) {
        let Some(path) = capture.get(1).map(|value| value.as_str()) else {
            continue;
        };
        let Ok(url) = base.join(path) else {
            continue;
        };
        let Some(name) = url
            .path_segments()
            .and_then(|mut parts| parts.next_back())
            .and_then(safe_filename)
        else {
            continue;
        };
        result.push((name, url.to_string()));
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::{
        GithubAsset, GithubRelease, eligible_atom_version, eligible_release, matching_artifacts,
        release_assets_match,
    };

    fn release(tag: &str, draft: bool, prerelease: bool, assets: &[&str]) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_owned(),
            draft,
            prerelease,
            assets: assets
                .iter()
                .map(|name| GithubAsset {
                    name: (*name).to_owned(),
                    browser_download_url: format!(
                        "https://github.com/owner/repo/releases/download/{tag}/{name}"
                    ),
                })
                .collect(),
        }
    }

    #[test]
    fn skips_api_prereleases_unless_the_tool_opts_in() {
        let required = [Regex::new(r"^tool\.zip$").unwrap()];
        let ignored = Vec::new();
        let releases = || {
            vec![
                release("v2.0.0-rc1", false, true, &["tool.zip"]),
                release("v1.9.0", false, false, &["tool.zip"]),
            ]
        };

        assert_eq!(
            eligible_release(releases(), &required, &ignored, false)
                .unwrap()
                .tag_name,
            "v1.9.0"
        );
        assert_eq!(
            eligible_release(releases(), &required, &ignored, true)
                .unwrap()
                .tag_name,
            "v2.0.0-rc1"
        );
    }

    #[test]
    fn skips_atom_prerelease_tags_unless_the_tool_opts_in() {
        let ignored = Vec::new();

        assert!(!eligible_atom_version("v2.0.0-rc.1", &ignored, false));
        assert!(eligible_atom_version("v2.0.0-rc.1", &ignored, true));
        assert!(eligible_atom_version("v2.0.0", &ignored, false));
        assert!(eligible_atom_version("nightly-20260901", &ignored, false));
    }

    #[test]
    fn collects_every_asset_matching_a_plural_pattern() {
        let regex = Regex::new(r"^frida-server-.+\.xz$").unwrap();
        let artifacts = matching_artifacts(
            &regex,
            [
                ("frida-server-1-android-arm64.xz", "https://example/arm64"),
                ("frida-server-1-linux-x86_64.xz", "https://example/x64"),
                (
                    "frida-gadget-1-android-arm64.so.xz",
                    "https://example/gadget",
                ),
            ],
        );

        assert_eq!(artifacts.len(), 2);
        assert_eq!(
            artifacts[0].filename.as_deref(),
            Some("frida-server-1-android-arm64.xz")
        );
        assert_eq!(
            artifacts[1].filename.as_deref(),
            Some("frida-server-1-linux-x86_64.xz")
        );
    }

    #[test]
    fn requires_every_configured_asset_pattern_to_match_the_same_release() {
        let required = [
            Regex::new(r"windows.*\.zip$").unwrap(),
            Regex::new(r"linux.*\.tar\.gz$").unwrap(),
        ];
        assert!(release_assets_match(
            &required,
            ["windows-x64.zip", "linux-x64.tar.gz"]
        ));
        assert!(!release_assets_match(&required, ["windows-x64.zip"]));
    }
}
