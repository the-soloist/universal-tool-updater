use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, LINK};
use serde::Deserialize;
use url::Url;

use crate::domain::{ArtifactConfig, ResolvedArtifact, ResolvedRelease, Tool};
use crate::error::UpdaterError;
use crate::paths::{filename_from_url, safe_filename};

use super::util::{expected_sha256, get_text, incompatible_artifact};

type HtmlAssets = Vec<(String, String)>;

/// Upper bound on expanded_assets page probes per atom resolution, so asset
/// renames across recent releases cannot degrade resolution into long probe
/// runs against GitHub.
const MAX_ATOM_PROBE_VERSIONS: usize = 5;

/// Per-run memo of expanded_assets pages already fetched for a repository and
/// version, so asset probing across tools sharing a repository does not
/// re-request the same release page.
#[derive(Default)]
pub(super) struct HtmlAssetCache {
    entries: Mutex<HashMap<String, HashMap<String, HtmlAssets>>>,
}

impl HtmlAssetCache {
    fn get_or_insert_with(
        &self,
        repository: &str,
        version: &str,
        fetch: impl FnOnce() -> Result<HtmlAssets>,
    ) -> Result<HtmlAssets> {
        if let Some(assets) = self
            .lock()
            .get(repository)
            .and_then(|versions| versions.get(version))
        {
            return Ok(assets.clone());
        }
        let assets = fetch()?;
        self.lock()
            .entry(repository.to_owned())
            .or_default()
            .insert(version.to_owned(), assets.clone());
        Ok(assets)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, HashMap<String, HtmlAssets>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Bundles the per-tool context needed to fetch expanded_assets pages, so the
/// cache short-circuits repeated lookups for the same repository and version.
struct HtmlAssetSource<'a> {
    client: &'a Client,
    cache: &'a HtmlAssetCache,
    tool: &'a Tool,
    repository: &'a str,
}

impl HtmlAssetSource<'_> {
    fn assets(&self, version: &str) -> Result<HtmlAssets> {
        self.cache.get_or_insert_with(self.repository, version, || {
            assets_from_html(self.client, self.tool, self.repository, version)
        })
    }
}

pub(super) fn resolve(
    client: &Client,
    token: Option<&str>,
    cache: &HtmlAssetCache,
    tool: &Tool,
    repository: &str,
    ignored: &[String],
    allow_prereleases: bool,
) -> Result<ResolvedRelease> {
    let source = HtmlAssetSource {
        client,
        cache,
        tool,
        repository,
    };
    let (version, api_assets, mut html_assets) = if let Some(token) = token {
        let (version, assets) =
            release_from_api(client, token, tool, repository, ignored, allow_prereleases)?;
        (version, assets, None)
    } else {
        let (version, assets) = version_from_atom(&source, ignored, allow_prereleases)?;
        (version, None, assets)
    };

    let mut artifacts = Vec::new();
    for artifact in &tool.artifacts {
        match artifact {
            ArtifactConfig::GithubAsset { pattern, .. } => {
                let regex = asset_regex(tool, pattern)?;
                let pin = expected_sha256(artifact, &version);
                let resolved = matching_github_assets(
                    &source,
                    &version,
                    &api_assets,
                    &mut html_assets,
                    &regex,
                    pin.as_deref(),
                )?
                .into_iter()
                .next()
                .ok_or_else(|| UpdaterError::Resolution {
                    tool: tool.id.clone(),
                    message: format!("no GitHub asset matched {pattern:?}"),
                })?;
                artifacts.push(resolved);
            }
            ArtifactConfig::GithubAssets { pattern, .. } => {
                let regex = asset_regex(tool, pattern)?;
                let pin = expected_sha256(artifact, &version);
                let resolved = matching_github_assets(
                    &source,
                    &version,
                    &api_assets,
                    &mut html_assets,
                    &regex,
                    pin.as_deref(),
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
            ArtifactConfig::GithubSource { format, .. } => {
                let url =
                    format!("https://github.com/{repository}/archive/refs/tags/{version}.{format}");
                artifacts.push(ResolvedArtifact {
                    filename: filename_from_url(&url),
                    url,
                    expected_sha256: expected_sha256(artifact, &version),
                });
            }
            ArtifactConfig::DirectUrl { url, .. } => artifacts.push(ResolvedArtifact {
                filename: filename_from_url(url),
                url: url.clone(),
                expected_sha256: expected_sha256(artifact, &version),
            }),
            ArtifactConfig::UrlTemplate { url, .. } => {
                let url = url.replace("{version}", &version);
                artifacts.push(ResolvedArtifact {
                    filename: filename_from_url(&url),
                    url,
                    expected_sha256: expected_sha256(artifact, &version),
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
    source: &HtmlAssetSource<'_>,
    version: &str,
    api_assets: &Option<Vec<GithubAsset>>,
    html_assets: &mut Option<HtmlAssets>,
    regex: &Regex,
    expected: Option<&str>,
) -> Result<Vec<ResolvedArtifact>> {
    if let Some(assets) = api_assets {
        return Ok(matching_artifacts(
            regex,
            expected,
            assets
                .iter()
                .map(|asset| (asset.name.as_str(), asset.browser_download_url.as_str())),
        ));
    }
    if html_assets.is_none() {
        *html_assets = Some(source.assets(version)?);
    }
    Ok(matching_artifacts(
        regex,
        expected,
        html_assets
            .as_ref()
            .expect("GitHub HTML assets were initialized")
            .iter()
            .map(|(name, url)| (name.as_str(), url.as_str())),
    ))
}

fn matching_artifacts<'a>(
    regex: &Regex,
    expected: Option<&str>,
    assets: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<ResolvedArtifact> {
    assets
        .into_iter()
        .filter(|(name, _)| regex.is_match(name))
        .map(|(name, url)| ResolvedArtifact {
            url: url.to_owned(),
            filename: Some(name.to_owned()),
            expected_sha256: expected.map(ToOwned::to_owned),
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
    let required = required_asset_patterns(tool)?;
    let mut page_url = Some(format!(
        "https://api.github.com/repos/{repository}/releases?per_page=30"
    ));
    // When ignore/prerelease/draft/asset filters exhaust a page, follow the
    // Link header for at most one extra page (60 releases total) before
    // declaring nothing eligible.
    for _ in 0..2 {
        let Some(url) = page_url.take() else {
            break;
        };
        let (releases, next_page) = fetch_release_page(client, token, tool, &url)?;
        if let Some(release) = eligible_release(releases, &required, ignored, allow_prereleases) {
            return Ok((release.tag_name, Some(release.assets)));
        }
        page_url = next_page;
    }
    Err(UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: "no eligible GitHub release matched the configured assets".to_owned(),
    }
    .into())
}

fn fetch_release_page(
    client: &Client,
    token: &str,
    tool: &Tool,
    url: &str,
) -> Result<(Vec<GithubRelease>, Option<String>)> {
    let response = client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("GitHub API request failed: {error}"),
        })?;
    let next = next_page_url(
        response
            .headers()
            .get(LINK)
            .and_then(|value| value.to_str().ok()),
    );
    let releases = response.json().map_err(|error| UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("invalid GitHub API response: {error}"),
    })?;
    Ok((releases, next))
}

/// Extracts the `rel="next"` target from a GitHub API Link header, keeping
/// only https URLs on the api.github.com host.
fn next_page_url(link: Option<&str>) -> Option<String> {
    let segment = link?.split(',').find(|segment| {
        segment
            .split_once(';')
            .is_some_and(|(_, rel)| rel.trim().eq_ignore_ascii_case(r#"rel="next""#))
    })?;
    let (url, _) = segment.split_once(';')?;
    let url = url.trim().trim_matches(['<', '>']);
    let parsed = Url::parse(url).ok()?;
    (parsed.scheme() == "https" && parsed.host_str() == Some("api.github.com"))
        .then(|| url.to_owned())
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

static ATOM_TAG_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"/releases/tag/([^\"<]+)"#).expect("static regex"));

fn version_from_atom(
    source: &HtmlAssetSource<'_>,
    ignored: &[String],
    allow_prereleases: bool,
) -> Result<(String, Option<HtmlAssets>)> {
    let tool = source.tool;
    let repository = source.repository;
    let url = format!("https://github.com/{repository}/releases.atom");
    let body = get_text(source.client, tool, &url)?;
    let versions = ATOM_TAG_PATTERN
        .captures_iter(&body)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned()))
        .filter(|version| eligible_atom_version(version, ignored, allow_prereleases));
    let required = required_asset_patterns(tool)?;
    probe_atom_versions(versions, |version| source.assets(version), &required)?.ok_or_else(|| {
        UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: "no eligible GitHub release matched the configured assets".to_owned(),
        }
        .into()
    })
}

/// Probes at most `MAX_ATOM_PROBE_VERSIONS` candidate versions; the first one
/// whose expanded_assets page satisfies every required pattern wins. A failed
/// page fetch aborts the probe instead of moving on to the next version.
fn probe_atom_versions(
    versions: impl IntoIterator<Item = String>,
    mut fetch_assets: impl FnMut(&str) -> Result<HtmlAssets>,
    required: &[Regex],
) -> Result<Option<(String, Option<HtmlAssets>)>> {
    for version in versions.into_iter().take(MAX_ATOM_PROBE_VERSIONS) {
        if required.is_empty() {
            return Ok(Some((version, None)));
        }
        let assets = fetch_assets(&version)?;
        if release_assets_match(required, assets.iter().map(|(name, _)| name.as_str())) {
            return Ok(Some((version, Some(assets))));
        }
    }
    Ok(None)
}

fn required_asset_patterns(tool: &Tool) -> Result<Vec<Regex>> {
    tool.artifacts
        .iter()
        .filter_map(|artifact| match artifact {
            ArtifactConfig::GithubAsset { pattern, .. }
            | ArtifactConfig::GithubAssets { pattern, .. } => Some(asset_regex(tool, pattern)),
            _ => None,
        })
        .collect()
}

/// Atom feeds carry no prerelease flag, so candidate tags are screened with
/// semver instead: a tag that parses (with an optional `v` prefix) and carries
/// a pre-release segment is skipped unless the tool opted in. Tags that do
/// not parse as semver are kept as before.
fn eligible_atom_version(version: &str, ignored: &[String], allow_prereleases: bool) -> bool {
    !ignored.iter().any(|ignored| ignored == version)
        && (allow_prereleases || {
            let trimmed = version.strip_prefix('v').unwrap_or(version);
            match semver::Version::parse(trimmed) {
                Ok(parsed) => parsed.pre.is_empty(),
                Err(_) => true,
            }
        })
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
    Ok(assets_from_body(&body))
}

static HREF_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="([^"]+)""#).expect("static regex"));

/// Extracts asset links from an expanded_assets page, keeping only hrefs that
/// resolve onto the GitHub hosts that legitimately serve release downloads.
fn assets_from_body(body: &str) -> HtmlAssets {
    let href = &*HREF_PATTERN;
    let base = Url::parse("https://github.com").expect("static URL");
    let mut result = Vec::new();
    for capture in href.captures_iter(body) {
        let Some(path) = capture.get(1).map(|value| value.as_str()) else {
            continue;
        };
        let Ok(url) = base.join(path) else {
            continue;
        };
        if !is_allowed_asset_host(url.host_str()) {
            continue;
        }
        let Some(name) = url
            .path_segments()
            .and_then(|mut parts| parts.next_back())
            .and_then(safe_filename)
        else {
            continue;
        };
        result.push((name, url.to_string()));
    }
    result
}

fn is_allowed_asset_host(host: Option<&str>) -> bool {
    matches!(
        host,
        Some("github.com") | Some("objects.githubusercontent.com")
    )
}

#[derive(Debug, Clone, Deserialize)]
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
    use std::cell::Cell;

    use regex::Regex;

    use super::{
        GithubAsset, GithubRelease, HtmlAssetCache, MAX_ATOM_PROBE_VERSIONS, assets_from_body,
        eligible_atom_version, eligible_release, matching_artifacts, next_page_url,
        probe_atom_versions, release_assets_match,
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
    fn skips_prereleases_unless_the_tool_opts_in() {
        let required = [Regex::new(r"^tool\.zip$").unwrap()];
        let ignored = Vec::new();
        let releases = vec![
            release("v2.0.0-rc1", false, true, &["tool.zip"]),
            release("v1.9.0", false, false, &["tool.zip"]),
        ];

        let selected = eligible_release(releases.clone(), &required, &ignored, false).unwrap();
        assert_eq!(selected.tag_name, "v1.9.0");

        let selected = eligible_release(releases, &required, &ignored, true).unwrap();
        assert_eq!(selected.tag_name, "v2.0.0-rc1");
    }

    #[test]
    fn atom_versions_skip_prerelease_tags_unless_the_tool_opts_in() {
        let ignored: Vec<String> = Vec::new();
        let feed = ["1.0.0-rc1", "1.0.0"];

        let stable = feed
            .iter()
            .find(|version| eligible_atom_version(version, &ignored, false))
            .unwrap();
        assert_eq!(stable, &"1.0.0");

        let opted_in = feed
            .iter()
            .find(|version| eligible_atom_version(version, &ignored, true))
            .unwrap();
        assert_eq!(opted_in, &"1.0.0-rc1");
    }

    #[test]
    fn atom_versions_keep_tags_that_do_not_parse_as_semver() {
        let ignored: Vec<String> = Vec::new();
        assert!(eligible_atom_version("nightly-2026-08-29", &ignored, false));
        assert!(eligible_atom_version("v2.0.0", &ignored, false));
        assert!(!eligible_atom_version("v2.0.0-beta.3", &ignored, false));
        assert!(!eligible_atom_version("2.0.0-rc1+build", &ignored, false));
        assert!(eligible_atom_version("v2.0.0-beta.3", &ignored, true));

        let ignored = vec!["1.0.0".to_owned()];
        assert!(!eligible_atom_version("1.0.0", &ignored, true));
    }

    #[test]
    fn still_excludes_drafts_and_ignored_versions_when_prereleases_are_allowed() {
        let required = [Regex::new(r"^tool\.zip$").unwrap()];
        let ignored = vec!["v1.8.0".to_owned()];
        let releases = vec![
            release("v2.0.0-draft", true, false, &["tool.zip"]),
            release("v1.8.0", false, true, &["tool.zip"]),
            release("v1.7.0", false, true, &["tool.zip"]),
        ];

        let selected = eligible_release(releases, &required, &ignored, true).unwrap();
        assert_eq!(selected.tag_name, "v1.7.0");
    }

    #[test]
    fn keeps_only_github_hosts_from_expanded_assets_pages() {
        let body = r#"
            <a href="/owner/repo/releases/download/v1/tool.zip">tool.zip</a>
            <a href="https://objects.githubusercontent.com/github-production/v1.bin">v1.bin</a>
            <a href="https://evil.example.com/tool.zip">evil.zip</a>
            <a href="https://github.com.evil.example/tool.zip">evil.zip</a>
        "#;

        let assets = assets_from_body(body);
        let names = assets
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["tool.zip", "v1.bin"]);
    }

    #[test]
    fn caches_expanded_assets_per_repository_and_version() {
        let cache = HtmlAssetCache::default();
        let fetches = Cell::new(0usize);
        let counting = |asset: &'static str| {
            let fetches = &fetches;
            move || {
                fetches.set(fetches.get() + 1);
                Ok(vec![(
                    asset.to_owned(),
                    format!("https://github.com/owner/repo/releases/download/v1/{asset}"),
                )])
            }
        };

        let first = cache
            .get_or_insert_with("owner/repo", "v1", counting("a.zip"))
            .unwrap();
        let second = cache
            .get_or_insert_with("owner/repo", "v1", counting("a.zip"))
            .unwrap();
        assert_eq!(fetches.get(), 1);
        assert_eq!(first, second);

        cache
            .get_or_insert_with("owner/repo", "v2", counting("b.zip"))
            .unwrap();
        cache
            .get_or_insert_with("other/repo", "v1", counting("c.zip"))
            .unwrap();
        assert_eq!(fetches.get(), 3);
    }

    #[test]
    fn does_not_cache_failed_expanded_assets_fetches() {
        let cache = HtmlAssetCache::default();
        let attempts = Cell::new(0usize);

        let failure = cache.get_or_insert_with("owner/repo", "v1", || {
            attempts.set(attempts.get() + 1);
            Err(anyhow::anyhow!("fetch failed"))
        });
        assert!(failure.is_err());

        let assets = cache
            .get_or_insert_with("owner/repo", "v1", || {
                attempts.set(attempts.get() + 1);
                Ok(vec![(
                    "tool.zip".to_owned(),
                    "https://github.com/dl".to_owned(),
                )])
            })
            .unwrap();
        assert_eq!(attempts.get(), 2);
        assert_eq!(assets.len(), 1);
    }

    #[test]
    fn collects_every_asset_matching_a_plural_pattern() {
        let regex = Regex::new(r"^frida-server-.+\.xz$").unwrap();
        let artifacts = matching_artifacts(
            &regex,
            None,
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
    fn applies_a_configured_digest_to_every_matched_asset() {
        let regex = Regex::new(r"^tool-.+\.zip$").unwrap();
        let artifacts = matching_artifacts(
            &regex,
            Some(&"a".repeat(64)),
            [
                ("tool-win.zip", "https://example/win"),
                ("tool-linux.zip", "https://example/linux"),
            ],
        );

        assert_eq!(artifacts.len(), 2);
        for artifact in artifacts {
            assert_eq!(
                artifact.expected_sha256.as_deref(),
                Some("a".repeat(64).as_str())
            );
        }
    }

    #[test]
    fn atom_probing_is_capped_at_five_versions() {
        let required = [Regex::new(r"^tool\.zip$").unwrap()];
        let fetches = Cell::new(0usize);
        let count = |_: &str| {
            fetches.set(fetches.get() + 1);
            Ok(Vec::new())
        };

        let versions = (1..=7).map(|index| format!("v{index}.0.0"));
        let selected = probe_atom_versions(versions, count, &required).unwrap();

        assert!(selected.is_none());
        assert_eq!(
            fetches.get(),
            MAX_ATOM_PROBE_VERSIONS,
            "seven candidates must not exhaust the probe budget"
        );
    }

    #[test]
    fn atom_probing_aborts_on_a_failed_page_fetch() {
        let required = [Regex::new(r"^tool\.zip$").unwrap()];
        let fetches = Cell::new(0usize);
        let failing = |version: &str| {
            fetches.set(fetches.get() + 1);
            match version {
                "v1.0.0" => Ok(vec![(
                    "other.zip".to_owned(),
                    "https://github.com/dl".to_owned(),
                )]),
                _ => Err(anyhow::anyhow!("expanded_assets request failed")),
            }
        };

        let error = probe_atom_versions(
            [
                "v1.0.0".to_owned(),
                "v2.0.0".to_owned(),
                "v3.0.0".to_owned(),
            ],
            failing,
            &required,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("expanded_assets request failed"),
            "the page failure must surface, got {error:#}"
        );
        assert_eq!(
            fetches.get(),
            2,
            "probing must stop at the failed version instead of continuing"
        );
    }

    #[test]
    fn atom_probing_returns_the_first_version_without_fetching_when_no_pattern_is_required() {
        let fetches = Cell::new(0usize);
        let never = |_: &str| {
            fetches.set(fetches.get() + 1);
            Ok(Vec::new())
        };

        let (version, assets) =
            probe_atom_versions(["v2.0.0".to_owned(), "v1.0.0".to_owned()], never, &[])
                .unwrap()
                .unwrap();

        assert_eq!(version, "v2.0.0");
        assert!(assets.is_none());
        assert_eq!(fetches.get(), 0);
    }

    #[test]
    fn parses_next_page_urls_from_github_link_headers() {
        let header = r#"<https://api.github.com/repositories/1/releases?per_page=30&page=2>; rel="next", <https://api.github.com/repositories/1/releases?per_page=30&page=1>; rel="prev""#;
        assert_eq!(
            next_page_url(Some(header)).as_deref(),
            Some("https://api.github.com/repositories/1/releases?per_page=30&page=2")
        );

        let last_page =
            r#"<https://api.github.com/repositories/1/releases?per_page=30&page=1>; rel="prev""#;
        assert_eq!(next_page_url(Some(last_page)), None);
        assert_eq!(next_page_url(None), None);

        let offsite = r#"<https://evil.example.com/page2>; rel="next""#;
        assert_eq!(next_page_url(Some(offsite)), None);

        let plain_http = r#"<http://api.github.com/page2>; rel="next""#;
        assert_eq!(next_page_url(Some(plain_http)), None);
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
