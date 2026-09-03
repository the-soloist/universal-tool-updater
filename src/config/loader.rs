mod paths;
mod registry;
mod tool;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::AppConfig;
use crate::config::model::{ManifestFile, SCHEMA_VERSION, ToolFile};
use crate::config::validation::{validate_manifest_values, validate_tool_config};
use crate::error::UpdaterError;

use paths::{profile_name, resolve_include, resolve_manifest_path, resolve_paths};
use registry::ToolRegistry;

pub fn load(manifest_path: &Path) -> Result<AppConfig> {
    let app_root = std::env::current_dir().context("cannot determine the working directory")?;
    let manifest_path = resolve_manifest_path(&app_root, manifest_path);
    let manifest: ManifestFile = read_yaml(&manifest_path)?;
    validate_manifest(&manifest_path, &manifest)?;
    let paths = resolve_paths(&app_root, &manifest_path, &manifest)?;

    let manifest_dir = manifest_path.parent().unwrap_or(Path::new("."));
    let mut registry = ToolRegistry::default();
    for include in &manifest.include {
        let path = resolve_include(&manifest_path, manifest_dir, include)?;
        let profile = profile_name(&path)?;
        registry.add_profile(&manifest_path, &path, &profile)?;
        let tool_file: ToolFile = read_yaml(&path)?;
        if tool_file.tools.is_empty() {
            return Err(UpdaterError::config(&path, "tools must not be empty").into());
        }
        for (id, raw) in tool_file.tools {
            validate_tool_config(
                &path,
                &id,
                &raw,
                &app_root,
                manifest.allow_insecure_transports,
            )?;
            registry.ensure_unique_id(&path, &id)?;
            let tool =
                tool::materialize(&path, id, profile.clone(), raw, &manifest.defaults, &paths)?;
            registry.insert(&path, tool)?;
        }
    }
    let tools = registry.finish();

    Ok(AppConfig {
        app_root,
        paths,
        network: manifest.network,
        allow_insecure_transports: manifest.allow_insecure_transports,
        tools,
    })
}

fn validate_manifest(path: &Path, manifest: &ManifestFile) -> Result<()> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(UpdaterError::config(
            path,
            format!(
                "unsupported schema version {}; expected {SCHEMA_VERSION}",
                manifest.schema_version
            ),
        )
        .into());
    }
    if manifest.include.is_empty() {
        return Err(UpdaterError::config(path, "include must not be empty").into());
    }
    validate_manifest_values(path, manifest)
}

fn read_yaml<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let input = fs::read_to_string(path)
        .with_context(|| format!("cannot read configuration {}", path.display()))?;
    yaml_serde::from_str(&input)
        .map_err(|error| UpdaterError::config(path, format!("invalid YAML: {error}")).into())
}
