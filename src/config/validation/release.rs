use std::path::Path;

use anyhow::Result;
use regex::Regex;

use crate::config::model::{ArtifactConfig, ReleaseConfig};
use crate::error::UpdaterError;

pub(super) fn validate(path: &Path, id: &str, release: &ReleaseConfig) -> Result<()> {
    match release {
        ReleaseConfig::Github { repository, .. } => {
            let mut parts = repository.split('/');
            if parts.next().filter(|part| !part.is_empty()).is_none()
                || parts.next().filter(|part| !part.is_empty()).is_none()
                || parts.next().is_some()
            {
                return Err(UpdaterError::config(
                    path,
                    format!("tool {id}: GitHub repository must be owner/name"),
                )
                .into());
            }
        }
        ReleaseConfig::Web {
            url,
            version_pattern,
            ..
        } => {
            validate_url(path, id, url)?;
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
        }
    }
    Ok(())
}

pub(super) fn validate_artifacts(
    path: &Path,
    id: &str,
    release: &ReleaseConfig,
    artifacts: &[ArtifactConfig],
) -> Result<()> {
    if artifacts.is_empty() {
        return Err(
            UpdaterError::config(path, format!("tool {id}: artifacts must not be empty")).into(),
        );
    }
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
                validate_url(path, id, &url.replace("{version}", "v1.0.0"))?;
            }
            ArtifactConfig::ReleaseUrl => require_release_type(
                path,
                id,
                matches!(release, ReleaseConfig::Http { .. }),
                "release-url requires an HTTP release",
            )?,
        }
    }
    Ok(())
}

fn require_release_type(path: &Path, id: &str, valid: bool, message: &str) -> Result<()> {
    if !valid {
        return Err(UpdaterError::config(path, format!("tool {id}: {message}")).into());
    }
    Ok(())
}

fn validate_regex(path: &Path, id: &str, field: &str, pattern: &str) -> Result<()> {
    Regex::new(pattern).map_err(|error| {
        UpdaterError::config(path, format!("tool {id}: invalid {field} regex: {error}"))
    })?;
    Ok(())
}

fn validate_url(path: &Path, id: &str, value: &str) -> Result<()> {
    url::Url::parse(value).map_err(|error| {
        UpdaterError::config(path, format!("tool {id}: invalid URL {value:?}: {error}"))
    })?;
    Ok(())
}
