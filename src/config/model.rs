use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::archive::ExtractionLimits;
pub use crate::domain::{
    ArtifactConfig, ExistingPolicy, HookAction, HookConfig, HookWorkingDirectory, InputMode,
    NetworkConfig, OutputMode, ReleaseConfig,
};

pub const SCHEMA_VERSION: u32 = 5;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub schema_version: u32,
    pub include: Vec<String>,
    pub paths: PathConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    /// 解压与下载的累计配额；省略时使用默认值 8 GiB / 100000 条目。
    #[serde(default, skip_serializing_if = "ExtractionLimits::is_default")]
    pub extraction_limits: ExtractionLimits,
}

/// 私有 YAML 模型：在反序列化边界承载 `allow_insecure_transports`，
/// 避免为新增配置扩展公开可构造的 `ManifestFile` 字段集（0.2.x 兼容线）。
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawManifest {
    pub schema_version: u32,
    pub include: Vec<String>,
    pub paths: PathConfig,
    /// Permits plain-HTTP download URLs; HTTPS remains the only default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_insecure_transports: bool,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    /// 解压与下载的累计配额；省略时使用默认值 8 GiB / 100000 条目。
    #[serde(default, skip_serializing_if = "ExtractionLimits::is_default")]
    pub extraction_limits: ExtractionLimits,
}

impl From<RawManifest> for ManifestFile {
    fn from(raw: RawManifest) -> Self {
        Self {
            schema_version: raw.schema_version,
            include: raw.include,
            paths: raw.paths,
            network: raw.network,
            defaults: raw.defaults,
            extraction_limits: raw.extraction_limits,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathConfig {
    pub toolkit_root: PathBuf,
    #[serde(default = "default_downloads")]
    pub downloads: PathBuf,
    /// Transaction workspace. Relative paths are resolved from the updater binary;
    /// when omitted it defaults to `<downloads>/staging`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging: Option<PathBuf>,
    #[serde(default = "default_state")]
    pub state: PathBuf,
}

fn default_downloads() -> PathBuf {
    PathBuf::from("updates")
}

fn default_state() -> PathBuf {
    PathBuf::from(".updater/state.yaml")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    pub create_destination: bool,
    pub install: InstallDefaults,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            create_destination: true,
            install: InstallDefaults::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstallDefaults {
    pub input: InputMode,
    pub existing: ExistingPolicy,
    pub save: OutputMode,
    pub strip_single_root: bool,
    pub archive_name: String,
}

impl Default for InstallDefaults {
    fn default() -> Self {
        Self {
            input: InputMode::Extract,
            existing: ExistingPolicy::Replace,
            save: OutputMode::Directory,
            strip_single_root: true,
            archive_name: "{name} - {version}.7z".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFile {
    pub tools: BTreeMap<String, ToolConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub release: ReleaseConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactConfig>,
    pub install: InstallConfig,
    #[serde(default, skip_serializing_if = "HookConfig::is_empty")]
    pub hooks: HookConfig,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstallConfig {
    pub destination: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing: Option<ExistingPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save: Option<OutputMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip_single_root: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_destination: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_password: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_symlinks_in_archive: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub executable: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symlinks: Vec<SymlinkConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymlinkConfig {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::{ManifestFile, RawManifest};

    const MINIMAL: &str = "
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
";

    #[test]
    fn raw_manifest_defaults_the_transport_flag_to_false_and_omits_it() {
        let raw: RawManifest = yaml_serde::from_str(MINIMAL).unwrap();
        assert!(!raw.allow_insecure_transports);
        assert!(
            !yaml_serde::to_string(&raw)
                .unwrap()
                .contains("allow_insecure_transports")
        );
    }

    #[test]
    fn raw_manifest_round_trips_an_explicit_transport_opt_in() {
        let raw: RawManifest =
            yaml_serde::from_str(&format!("{MINIMAL}allow_insecure_transports: true\n")).unwrap();
        assert!(raw.allow_insecure_transports);
        assert!(
            yaml_serde::to_string(&raw)
                .unwrap()
                .contains("allow_insecure_transports: true")
        );
    }

    #[test]
    fn manifest_file_strips_the_transport_flag_when_converted() {
        let raw: RawManifest =
            yaml_serde::from_str(&format!("{MINIMAL}allow_insecure_transports: true\n")).unwrap();
        let manifest = ManifestFile::from(raw);
        assert!(
            !yaml_serde::to_string(&manifest)
                .unwrap()
                .contains("allow_insecure_transports")
        );
    }
}
