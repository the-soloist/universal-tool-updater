use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) const VERSION_FILE: &str = ".version";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GithubTokenSource {
    Environment(String),
    GhAuthToken,
}

impl GithubTokenSource {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        if let Some(name) = value.strip_prefix("env:") {
            if is_portable_environment_name(name) {
                return Ok(Self::Environment(name.to_owned()));
            }
            return Err(
                "must use env:<name>, where <name> is a portable environment variable name"
                    .to_owned(),
            );
        }
        if value == "gh auth token" {
            return Ok(Self::GhAuthToken);
        }
        Err("must be env:<name> or 'gh auth token'".to_owned())
    }
}

fn is_portable_environment_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub user_agent: String,
    pub timeout_seconds: u64,
    pub progress: bool,
    pub github_token_source: String,
    pub jobs: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            user_agent: "Universal-Tool-Updater/3".to_owned(),
            timeout_seconds: 60,
            progress: true,
            github_token_source: "env:GITHUB_TOKEN".to_owned(),
            jobs: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputMode {
    #[default]
    Extract,
    Copy,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExistingPolicy {
    #[default]
    Replace,
    Merge,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OutputMode {
    #[default]
    Directory,
    Archive,
}

pub(crate) fn effective_output_mode(
    input: InputMode,
    configured: OutputMode,
    artifacts: &[ArtifactConfig],
) -> OutputMode {
    if input == InputMode::Copy
        && artifacts.iter().any(|artifact| {
            matches!(
                artifact,
                ArtifactConfig::GithubAsset { .. }
                    | ArtifactConfig::GithubAssets { .. }
                    | ArtifactConfig::GithubSource { .. }
            )
        })
    {
        OutputMode::Directory
    } else {
        configured
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ReleaseConfig {
    Github {
        repository: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ignore_versions: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        allow_prereleases: bool,
    },
    Web {
        url: String,
        version_pattern: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ignore_versions: Vec<String>,
    },
    Http {
        url: String,
        #[serde(default = "default_version_headers")]
        version_headers: Vec<String>,
    },
    /// A registered tool whose releases are maintained outside the updater.
    Manual {},
}

fn default_version_headers() -> Vec<String> {
    vec![
        "etag".to_owned(),
        "last-modified".to_owned(),
        "content-length".to_owned(),
    ]
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ArtifactConfig {
    GithubAsset {
        pattern: String,
    },
    GithubAssets {
        pattern: String,
    },
    GithubSource {
        format: String,
    },
    PageLink {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
    DirectUrl {
        url: String,
    },
    UrlTemplate {
        url: String,
    },
    ReleaseUrl,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub before_update: Vec<HookAction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after_unpack: Vec<HookAction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after_install: Vec<HookAction>,
}

impl HookConfig {
    pub fn is_empty(&self) -> bool {
        self.before_update.is_empty()
            && self.after_unpack.is_empty()
            && self.after_install.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HookAction {
    Rename {
        from: String,
        to: PathBuf,
    },
    MoveContents {
        from: PathBuf,
        to: PathBuf,
    },
    Python {
        script: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(
            default = "default_hook_timeout_seconds",
            skip_serializing_if = "is_default_hook_timeout_seconds"
        )]
        timeout_seconds: u64,
        #[serde(default, skip_serializing_if = "is_default")]
        working_directory: HookWorkingDirectory,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        environment: BTreeMap<String, String>,
    },
}

fn default_hook_timeout_seconds() -> u64 {
    300
}

fn is_default_hook_timeout_seconds(value: &u64) -> bool {
    *value == default_hook_timeout_seconds()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HookWorkingDirectory {
    App,
    #[default]
    Toolkit,
    Downloads,
    Staging,
    Install,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

#[derive(Debug, Clone)]
pub struct InstallSpec {
    pub destination: PathBuf,
    pub input: InputMode,
    pub existing: ExistingPolicy,
    pub save: OutputMode,
    pub strip_single_root: bool,
    pub create_destination: bool,
    pub archive_name: String,
    pub archive_password: Option<String>,
    /// Opt-in flag: archives containing symbolic or hard links are rejected by default.
    pub allow_symlinks_in_archive: bool,
    pub executable: Vec<PathBuf>,
    pub symlinks: Vec<SymlinkSpec>,
}

#[derive(Debug, Clone)]
pub struct SymlinkSpec {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub enabled: bool,
    pub allow_insecure_transports: bool,
    pub release: ReleaseConfig,
    pub artifacts: Vec<ArtifactConfig>,
    pub install: InstallSpec,
    pub hooks: HookConfig,
}

impl Tool {
    pub(crate) fn version_marker_path(&self) -> Option<PathBuf> {
        (effective_output_mode(self.install.input, self.install.save, &self.artifacts)
            == OutputMode::Directory)
            .then(|| {
                if self.install.input == InputMode::Copy {
                    self.install
                        .destination
                        .parent()
                        .expect("an installation destination always has a parent")
                        .join(VERSION_FILE)
                } else {
                    self.install.destination.join(VERSION_FILE)
                }
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtifact {
    pub url: String,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRelease {
    pub version: String,
    pub artifacts: Vec<ResolvedArtifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    Updated,
    Current,
    Skipped,
    Failed,
    Planned,
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub tool_id: String,
    pub status: UpdateStatus,
    pub version: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct DownloadedArtifact {
    pub path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::GithubTokenSource;

    #[test]
    fn parses_environment_token_sources() {
        assert_eq!(
            GithubTokenSource::parse("env:GITHUB_TOKEN"),
            Ok(GithubTokenSource::Environment("GITHUB_TOKEN".to_owned()))
        );
    }

    #[test]
    fn parses_gh_cli_token_source() {
        assert_eq!(
            GithubTokenSource::parse("gh auth token"),
            Ok(GithubTokenSource::GhAuthToken)
        );
    }

    #[test]
    fn rejects_ambiguous_token_sources() {
        assert!(GithubTokenSource::parse("GITHUB_TOKEN").is_err());
        assert!(GithubTokenSource::parse("env:token-name").is_err());
    }
}
