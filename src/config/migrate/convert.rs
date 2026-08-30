use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use toml::Value;

use crate::config::model::{
    ArtifactConfig, EnvironmentMode, ExistingPolicy, HookAction, HookConfig, HookWorkingDirectory,
    InputMode, InstallConfig, OutputMode, ReleaseConfig, SCHEMA_VERSION, SymlinkConfig, ToolConfig,
};

pub(super) fn convert_tool(
    name: &str,
    legacy: &toml::map::Map<String, Value>,
) -> Result<ToolConfig> {
    let source_type = string(legacy, "from").unwrap_or("web");
    let ignored = strings(legacy, "pass_version");
    let release = match source_type {
        "github" => ReleaseConfig::Github {
            repository: required_string(legacy, "url", name)?.to_owned(),
            ignore_versions: ignored,
            allow_prereleases: false,
        },
        "web" | "format" => ReleaseConfig::Web {
            url: required_string(legacy, "url", name)?.to_owned(),
            version_pattern: required_string(legacy, "re_version", name)?.to_owned(),
            ignore_versions: ignored,
        },
        "http" => ReleaseConfig::Http {
            url: required_string(legacy, "update_url", name)?.to_owned(),
            version_headers: vec![
                "etag".to_owned(),
                "last-modified".to_owned(),
                "content-length".to_owned(),
            ],
        },
        other => bail!("tool {name}: unsupported legacy source {other:?}"),
    };

    let mut artifacts = artifacts(legacy, name, source_type)?;
    if let Some(format) = string(legacy, "release_src") {
        artifacts.push(ArtifactConfig::GithubSource {
            format: format.to_owned(),
            sha256: None,
        });
    }
    if artifacts.is_empty() {
        bail!("tool {name}: legacy configuration resolves no artifacts");
    }

    let is_release_bundle = boolean(legacy, "is_release").unwrap_or(false);
    let destination = normalize_destination(required_string(legacy, "folder", name)?);
    let install = InstallConfig {
        destination,
        input: (!boolean(legacy, "unpack").unwrap_or(true)).then_some(InputMode::Copy),
        existing: (!is_release_bundle && boolean(legacy, "merge").unwrap_or(false))
            .then_some(ExistingPolicy::Merge),
        save: boolean(legacy, "repack")
            .unwrap_or(false)
            .then_some(OutputMode::Archive),
        strip_single_root: None,
        create_destination: None,
        archive_name: None,
        archive_password: string(legacy, "update_file_pass").map(ToOwned::to_owned),
        executable: strings(legacy, "executable_file")
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        allow_symlinks_in_archive: false,
        symlinks: symlinks(legacy, name)?,
    };
    let hooks = HookConfig {
        before_update: hook(legacy, "pre_update", name)?,
        after_unpack: hook(legacy, "post_unpack", name)?,
        after_install: hook(legacy, "post_update", name)?,
    };
    Ok(ToolConfig {
        name: Some(name.to_owned()),
        enabled: true,
        release,
        artifacts,
        install,
        hooks,
    })
}

fn artifacts(
    legacy: &toml::map::Map<String, Value>,
    name: &str,
    source_type: &str,
) -> Result<Vec<ArtifactConfig>> {
    Ok(match source_type {
        "github" => strings(legacy, "re_download")
            .into_iter()
            .map(|pattern| ArtifactConfig::GithubAsset {
                pattern,
                sha256: None,
            })
            .collect(),
        "web" => {
            let patterns = strings(legacy, "re_download");
            if patterns.is_empty() {
                vec![ArtifactConfig::DirectUrl {
                    url: required_string(legacy, "update_url", name)?.to_owned(),
                    sha256: None,
                }]
            } else {
                let base_url = string(legacy, "update_url").map(ToOwned::to_owned);
                patterns
                    .into_iter()
                    .map(|pattern| ArtifactConfig::PageLink {
                        pattern,
                        base_url: base_url.clone(),
                    })
                    .collect()
            }
        }
        "format" => strings(legacy, "format_url")
            .into_iter()
            .map(|url| ArtifactConfig::UrlTemplate { url, sha256: None })
            .collect(),
        "http" => vec![ArtifactConfig::ReleaseUrl],
        _ => unreachable!("source type validated while converting release"),
    })
}

fn symlinks(legacy: &toml::map::Map<String, Value>, name: &str) -> Result<Vec<SymlinkConfig>> {
    legacy
        .get("symlink")
        .and_then(Value::as_table)
        .map(|link| {
            Ok(vec![SymlinkConfig {
                from: PathBuf::from(required_string(link, "from", name)?),
                to: normalize_symlink_target(required_string(link, "to", name)?),
            }])
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn hook(table: &toml::map::Map<String, Value>, field: &str, tool: &str) -> Result<Vec<HookAction>> {
    let Some(script) = string(table, field) else {
        return Ok(Vec::new());
    };
    let script = PathBuf::from(script);
    if script
        .extension()
        .and_then(|extension| extension.to_str())
        .is_none_or(|extension| !extension.eq_ignore_ascii_case("py"))
    {
        bail!(
            "tool {tool}: legacy hook {field} uses {}; schema v{SCHEMA_VERSION} only permits Python external scripts; convert simple file operations to native actions",
            script.display()
        );
    }
    Ok(vec![HookAction::Python {
        script,
        args: Vec::new(),
        timeout_seconds: 300,
        working_directory: HookWorkingDirectory::Toolkit,
        environment_mode: EnvironmentMode::default(),
        environment: BTreeMap::new(),
    }])
}

pub(super) fn normalize_destination(value: &str) -> PathBuf {
    let normalized = value.replace('\\', "/");
    // String concatenation keeps the portable forward-slash form on Windows,
    // where PathBuf::join would re-serialize with a platform separator.
    if let Some(value) = normalized.strip_prefix("../../") {
        return PathBuf::from(value);
    }
    if let Some(value) = normalized.strip_prefix("/opt/tools/") {
        return PathBuf::from(format!("Tools/{value}"));
    }
    if let Some(value) = normalized.strip_prefix("/opt/apps/") {
        return PathBuf::from(format!("Apps/{value}"));
    }
    if let Some(value) = normalized.strip_prefix("/opt/") {
        return PathBuf::from(value);
    }
    PathBuf::from(normalized)
}

pub(super) fn normalize_symlink_target(value: &str) -> PathBuf {
    let normalized = value.replace('\\', "/");
    normalized
        .strip_prefix("/opt/binary/")
        .map(|value| PathBuf::from(format!("bin/{value}")))
        .unwrap_or_else(|| PathBuf::from(normalized))
}

pub(super) fn tool_id(name: &str, table: &toml::map::Map<String, Value>) -> String {
    let name_slug = slug(name);
    if name_slug
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .count()
        >= 3
    {
        return name_slug;
    }
    string(table, "url")
        .and_then(|value| value.trim_end_matches('/').rsplit('/').next())
        .map(slug)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "tool".to_owned())
}

pub(super) fn slug(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut separator = false;
    for (index, character) in characters.iter().copied().enumerate() {
        if character.is_ascii_alphanumeric() {
            let previous = index.checked_sub(1).and_then(|index| characters.get(index));
            let next = characters.get(index + 1);
            let camel_case_boundary = character.is_ascii_uppercase()
                && previous.is_some_and(|previous| {
                    previous.is_ascii_lowercase()
                        || previous.is_ascii_digit()
                        || (previous.is_ascii_uppercase()
                            && next.is_some_and(char::is_ascii_lowercase))
                });
            if (separator || camel_case_boundary) && !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    output.trim_matches('-').to_owned()
}

fn required_string<'a>(
    table: &'a toml::map::Map<String, Value>,
    field: &str,
    tool: &str,
) -> Result<&'a str> {
    string(table, field).ok_or_else(|| anyhow::anyhow!("tool {tool}: missing legacy field {field}"))
}

fn string<'a>(table: &'a toml::map::Map<String, Value>, field: &str) -> Option<&'a str> {
    table.get(field).and_then(Value::as_str)
}

fn strings(table: &toml::map::Map<String, Value>, field: &str) -> Vec<String> {
    table
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn boolean(table: &toml::map::Map<String, Value>, field: &str) -> Option<bool> {
    table.get(field).and_then(Value::as_bool)
}
