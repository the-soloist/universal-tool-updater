use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

const STATE_VERSION: u32 = 1;

#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    data: StateFile,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct StateFile {
    #[serde(default = "state_version")]
    schema_version: u32,
    #[serde(default)]
    tools: BTreeMap<String, ToolState>,
}

fn state_version() -> u32 {
    STATE_VERSION
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolState {
    pub version: String,
    pub updated_at: u64,
}

impl StateStore {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let data = match fs::read_to_string(&path) {
            Ok(input) => {
                let state: StateFile = yaml_serde::from_str(&input)
                    .with_context(|| format!("invalid state file {}", path.display()))?;
                if state.schema_version != STATE_VERSION {
                    anyhow::bail!(
                        "unsupported state schema {} in {}; expected {STATE_VERSION}",
                        state.schema_version,
                        path.display()
                    );
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StateFile {
                schema_version: STATE_VERSION,
                tools: BTreeMap::new(),
            },
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read state file {}", path.display()));
            }
        };
        Ok(Self { path, data })
    }

    pub fn version(&self, tool_id: &str) -> Option<&str> {
        self.data
            .tools
            .get(tool_id)
            .map(|entry| entry.version.as_str())
    }

    pub fn record(&mut self, tool_id: &str, version: &str) -> Result<()> {
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system time is before Unix epoch")?
            .as_secs();
        let previous = self.data.tools.insert(
            tool_id.to_owned(),
            ToolState {
                version: version.to_owned(),
                updated_at,
            },
        );
        if let Err(error) = self.save() {
            if let Some(previous) = previous {
                self.data.tools.insert(tool_id.to_owned(), previous);
            } else {
                self.data.tools.remove(tool_id);
            }
            return Err(error);
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create state directory {}", parent.display()))?;
        let encoded = yaml_serde::to_string(&self.data).context("cannot encode updater state")?;
        let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
            format!("cannot create temporary state file in {}", parent.display())
        })?;
        temporary
            .write_all(encoded.as_bytes())
            .context("cannot write temporary state file")?;
        temporary
            .as_file()
            .sync_all()
            .context("cannot sync temporary state file")?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("cannot replace state file {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::StateStore;

    #[test]
    fn persists_versions_atomically() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.yaml");
        let mut state = StateStore::load(&path).unwrap();
        state.record("bat", "v1.2.3").unwrap();

        let reloaded = StateStore::load(&path).unwrap();
        assert_eq!(reloaded.version("bat"), Some("v1.2.3"));
    }

    #[test]
    fn restores_in_memory_state_when_persistence_fails() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("state");
        let path = parent.join("state.yaml");
        let mut state = StateStore::load(&path).unwrap();
        std::fs::write(&parent, "blocks directory creation").unwrap();

        assert!(state.record("failed", "v1").is_err());
        assert_eq!(state.version("failed"), None);

        std::fs::remove_file(&parent).unwrap();
        state.record("working", "v2").unwrap();
        let reloaded = StateStore::load(path).unwrap();
        assert_eq!(reloaded.version("failed"), None);
        assert_eq!(reloaded.version("working"), Some("v2"));
    }
}
