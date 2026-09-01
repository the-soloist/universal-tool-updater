//! Conversion from the legacy flat TOML format to the current YAML schema.
//!
//! Legacy interpretation is deliberately isolated here. Runtime modules only
//! accept validated YAML configuration, so old aliases and boolean combinations
//! cannot leak into the update pipeline.

mod convert;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;
use toml::Value;

use crate::config::model::{
    DefaultsConfig, ManifestFile, NetworkConfig, PathConfig, SCHEMA_VERSION, ToolFile,
};

pub fn migrate_directory(input: &Path, output: &Path) -> Result<()> {
    if !input.is_dir() {
        bail!(
            "legacy configuration directory {} does not exist",
            input.display()
        );
    }
    fs::create_dir_all(output)
        .with_context(|| format!("cannot create migration output {}", output.display()))?;

    let files = legacy_files(input)?;
    let platform = input
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("platform");
    let mut used_ids = BTreeSet::new();
    let mut converted = Vec::new();
    let mut include = Vec::new();

    for path in files {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("cannot read legacy config {}", path.display()))?;
        let legacy: Value = toml::from_str(&source)
            .with_context(|| format!("invalid legacy config {}", path.display()))?;
        let table = legacy
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("{} is not a TOML table", path.display()))?;
        let mut tools = BTreeMap::new();
        for (name, value) in table {
            if name == "UpdaterConfig" || name == "UpdaterAutoUpdater" {
                continue;
            }
            let legacy_tool = value.as_table().ok_or_else(|| {
                anyhow::anyhow!("legacy tool {name} in {} is not a table", path.display())
            })?;
            let id = unique_tool_id(convert::tool_id(name, legacy_tool), &mut used_ids);
            tools.insert(id, convert::convert_tool(name, legacy_tool)?);
        }
        if tools.is_empty() {
            bail!("legacy config {} contains no tools", path.display());
        }
        let filename = path
            .with_extension("yaml")
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid filename {}", path.display()))?
            .to_owned();
        include.push(filename.to_string_lossy().into_owned());
        converted.push((output.join(filename), ToolFile { tools }));
    }

    for (path, file) in converted {
        write_yaml_atomic(&path, &file)?;
    }
    let manifest = ManifestFile {
        schema_version: SCHEMA_VERSION,
        include,
        paths: PathConfig {
            toolkit_root: PathBuf::from("~/Tools/Toolkit"),
            downloads: PathBuf::from("updates"),
            staging: None,
            state: PathBuf::from(format!(".updater/{platform}-state.yaml")),
        },
        network: NetworkConfig::default(),
        defaults: DefaultsConfig::default(),
        allow_insecure_transports: false,
    };
    write_yaml_atomic(&output.join("manifest.yaml"), &manifest)?;
    println!(
        "migrated {} configuration files to {}",
        manifest.include.len(),
        output.display()
    );
    Ok(())
}

fn legacy_files(input: &Path) -> Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(input)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| {
            path.extension().and_then(|value| value.to_str()) == Some("toml")
                && path.file_name().and_then(|value| value.to_str()) != Some("manifest.toml")
        })
        .collect::<Vec<_>>();
    files.sort();
    if files.is_empty() {
        bail!("no legacy TOML files found in {}", input.display());
    }
    Ok(files)
}

fn unique_tool_id(mut id: String, used: &mut BTreeSet<String>) -> String {
    let original = id.clone();
    let mut suffix = 2;
    while used.contains(&id) {
        id = format!("{original}-{suffix}");
        suffix += 1;
    }
    used.insert(id.clone());
    id
}

fn write_yaml_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let encoded = yaml_serde::to_string(value)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temporary.write_all(encoded.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}
