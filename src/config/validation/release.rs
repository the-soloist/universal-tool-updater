use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use regex::Regex;
use reqwest::header::HeaderName;

use crate::config::model::{ArtifactConfig, ReleaseConfig};
use crate::config::validation::validate_placeholders;
use crate::error::UpdaterError;

pub(super) fn validate(path: &Path, id: &str, release: &ReleaseConfig) -> Result<()> {
    match release {
        ReleaseConfig::Github {
            repository,
            ignore_versions,
            ..
        } => {
            let mut parts = repository.split('/');
            let owner = parts.next();
            let name = parts.next();
            if owner.is_none_or(|part| !valid_github_owner(part))
                || name.is_none_or(|part| !valid_github_repository(part))
                || parts.next().is_some()
            {
                return Err(UpdaterError::config(
                    path,
                    format!(
                        "tool {id}: GitHub repository must be a valid owner/name without whitespace"
                    ),
                )
                .into());
            }
            validate_ignored_versions(path, id, ignore_versions)?;
        }
        ReleaseConfig::Web {
            url,
            version_pattern,
            ignore_versions,
        } => {
            validate_url(path, id, url)?;
            if version_pattern.is_empty() {
                return Err(UpdaterError::config(
                    path,
                    format!("tool {id}: version_pattern must not be empty"),
                )
                .into());
            }
            let regex = Regex::new(version_pattern).map_err(|error| {
                UpdaterError::config(path, format!("tool {id}: invalid version regex: {error}"))
            })?;
            if regex.captures_len() < 2 {
                return Err(UpdaterError::config(
                    path,
                    format!("tool {id}: version regex needs a capture group"),
                )
                .into());
            }
            validate_ignored_versions(path, id, ignore_versions)?;
        }
        ReleaseConfig::Http {
            url,
            version_headers,
        } => {
            validate_url(path, id, url)?;
            if version_headers.is_empty() {
                return Err(UpdaterError::config(
                    path,
                    format!("tool {id}: version_headers must not be empty"),
                )
                .into());
            }
            let mut unique = BTreeSet::new();
            for header in version_headers {
                if HeaderName::from_bytes(header.as_bytes()).is_err() {
                    return Err(UpdaterError::config(
                        path,
                        format!("tool {id}: invalid HTTP version header {header:?}"),
                    )
                    .into());
                }
                let normalized = header.to_ascii_lowercase();
                if !unique.insert(normalized) {
                    return Err(UpdaterError::config(
                        path,
                        format!("tool {id}: duplicate HTTP version header {header:?}"),
                    )
                    .into());
                }
            }
        }
        ReleaseConfig::Manual {} => {}
    }
    Ok(())
}

pub(super) fn validate_artifacts(
    path: &Path,
    id: &str,
    release: &ReleaseConfig,
    artifacts: &[ArtifactConfig],
) -> Result<()> {
    if matches!(release, ReleaseConfig::Manual {}) {
        if !artifacts.is_empty() {
            return Err(UpdaterError::config(
                path,
                format!(
                    "tool {id}: manual tools are maintained manually and must not configure artifacts"
                ),
            )
            .into());
        }
        return Ok(());
    }
    if artifacts.is_empty() {
        return Err(
            UpdaterError::config(path, format!("tool {id}: artifacts must not be empty")).into(),
        );
    }
    let mut unique = BTreeSet::new();
    for artifact in artifacts {
        match artifact {
            ArtifactConfig::GithubAsset { pattern } => {
                require_release_type(
                    path,
                    id,
                    matches!(release, ReleaseConfig::Github { .. }),
                    "github-asset requires a GitHub release",
                )?;
                validate_regex(path, id, "asset", pattern)?;
            }
            ArtifactConfig::GithubAssets { pattern } => {
                require_release_type(
                    path,
                    id,
                    matches!(release, ReleaseConfig::Github { .. }),
                    "github-assets requires a GitHub release",
                )?;
                validate_regex(path, id, "assets", pattern)?;
            }
            ArtifactConfig::GithubSource { format } => {
                require_release_type(
                    path,
                    id,
                    matches!(release, ReleaseConfig::Github { .. }),
                    "github-source requires a GitHub release",
                )?;
                if !matches!(format.as_str(), "zip" | "tar.gz") {
                    return Err(UpdaterError::config(
                        path,
                        format!("tool {id}: unsupported GitHub source format {format}"),
                    )
                    .into());
                }
            }
            ArtifactConfig::PageLink { pattern, base_url } => {
                require_release_type(
                    path,
                    id,
                    matches!(release, ReleaseConfig::Web { .. }),
                    "page-link requires a web release",
                )?;
                validate_regex(path, id, "link", pattern)?;
                if let Some(base_url) = base_url {
                    validate_url(path, id, base_url)?;
                }
            }
            ArtifactConfig::DirectUrl { url } => validate_url(path, id, url)?,
            ArtifactConfig::UrlTemplate { url } => {
                if !url.contains("{version}") {
                    return Err(UpdaterError::config(
                        path,
                        format!("tool {id}: URL template must contain {{version}}"),
                    )
                    .into());
                }
                validate_placeholders(url, &["version"]).map_err(|message| {
                    UpdaterError::config(path, format!("tool {id}: URL template {message}"))
                })?;
                validate_url(path, id, &url.replace("{version}", "v1.0.0"))?;
            }
            ArtifactConfig::ReleaseUrl => require_release_type(
                path,
                id,
                matches!(release, ReleaseConfig::Http { .. }),
                "release-url requires an HTTP release",
            )?,
        }
        let key = artifact_key(artifact);
        if !unique.insert(key) {
            return Err(UpdaterError::config(
                path,
                format!("tool {id}: duplicate artifact configuration"),
            )
            .into());
        }
    }
    Ok(())
}

fn artifact_key(artifact: &ArtifactConfig) -> String {
    match artifact {
        ArtifactConfig::GithubAsset { pattern } => format!("github-asset\0{pattern}"),
        ArtifactConfig::GithubAssets { pattern } => format!("github-assets\0{pattern}"),
        ArtifactConfig::GithubSource { format } => format!("github-source\0{format}"),
        ArtifactConfig::PageLink { pattern, base_url } => {
            format!(
                "page-link\0{pattern}\0{}",
                base_url.as_deref().unwrap_or_default()
            )
        }
        ArtifactConfig::DirectUrl { url } => format!("direct-url\0{url}"),
        ArtifactConfig::UrlTemplate { url } => format!("url-template\0{url}"),
        ArtifactConfig::ReleaseUrl => "release-url".to_owned(),
    }
}

fn require_release_type(path: &Path, id: &str, valid: bool, message: &str) -> Result<()> {
    if !valid {
        return Err(UpdaterError::config(path, format!("tool {id}: {message}")).into());
    }
    Ok(())
}

fn validate_regex(path: &Path, id: &str, field: &str, pattern: &str) -> Result<()> {
    if pattern.is_empty() {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: {field} regex must not be empty"),
        )
        .into());
    }
    Regex::new(pattern).map_err(|error| {
        UpdaterError::config(path, format!("tool {id}: invalid {field} regex: {error}"))
    })?;
    Ok(())
}

fn validate_url(path: &Path, id: &str, value: &str) -> Result<()> {
    let parsed = url::Url::parse(value).map_err(|error| {
        UpdaterError::config(path, format!("tool {id}: invalid URL {value:?}: {error}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(UpdaterError::config(
            path,
            format!("tool {id}: URL {value:?} must use HTTP or HTTPS and include a host"),
        )
        .into());
    }
    Ok(())
}

fn validate_ignored_versions(path: &Path, id: &str, versions: &[String]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for version in versions {
        if version.is_empty() {
            return Err(UpdaterError::config(
                path,
                format!("tool {id}: ignore_versions entries must not be empty"),
            )
            .into());
        }
        if !unique.insert(version) {
            return Err(UpdaterError::config(
                path,
                format!("tool {id}: duplicate ignore_versions entry {version:?}"),
            )
            .into());
        }
    }
    Ok(())
}

fn valid_github_owner(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

fn valid_github_repository(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}
