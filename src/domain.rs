use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub user_agent: String,
    pub timeout_seconds: u64,
    pub progress: bool,
    pub github_token_env: String,
    pub jobs: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            user_agent: "Universal-Tool-Updater/3".to_owned(),
            timeout_seconds: 60,
            progress: true,
            github_token_env: "GITHUB_TOKEN".to_owned(),
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
}

fn default_version_headers() -> Vec<String> {
    vec![
        "etag".to_owned(),
        "last-modified".to_owned(),
        "content-length".to_owned(),
    ]
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
    pub release: ReleaseConfig,
    pub artifacts: Vec<ArtifactConfig>,
    pub install: InstallSpec,
    pub hooks: HookConfig,
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
