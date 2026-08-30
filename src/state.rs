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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<ArchiveState>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ArchiveState {
    // 大小和精确修改时间构成轻量文件身份；归档写入状态前已完成完整内容校验。
    size: u64,
    modified_seconds: u64,
    modified_nanoseconds: u32,
}

impl ArchiveState {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("cannot inspect installed archive {}", path.display()))?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("installed archive {} is not a regular file", path.display());
        }
        let modified = metadata
            .modified()
            .with_context(|| format!("cannot read modification time for {}", path.display()))?
            .duration_since(UNIX_EPOCH)
            .with_context(|| {
                format!(
                    "installed archive {} predates the Unix epoch",
                    path.display()
                )
            })?;
        Ok(Self {
            size: metadata.len(),
            modified_seconds: modified.as_secs(),
            modified_nanoseconds: modified.subsec_nanos(),
        })
    }

    pub(crate) fn matches(&self, path: &Path) -> bool {
        // 归档写入状态前已经完整解码校验；后续用文件身份快速识别是否被替换或改写。
        Self::capture(path).is_ok_and(|current| current == *self)
    }
}

/// One pending state transition collected during a run and persisted together
/// by [`StateStore::record_all`].
#[derive(Debug, Clone)]
pub(crate) enum StateRecord {
    /// A finished installation: the recorded version plus the identity of the
    /// installed archive when the tool produces one.
    Installation {
        version: String,
        archive: Option<ArchiveState>,
    },
    /// A verified archive for an already-recorded version; never changes the
    /// version and requires an existing entry.
    Archive(ArchiveState),
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

    pub(crate) fn archive(&self, tool_id: &str) -> Option<&ArchiveState> {
        self.data
            .tools
            .get(tool_id)
            .and_then(|entry| entry.archive.as_ref())
    }

    #[cfg(test)]
    pub fn record(&mut self, tool_id: &str, version: &str) -> Result<()> {
        self.record_all(&[(
            tool_id.to_owned(),
            StateRecord::Installation {
                version: version.to_owned(),
                archive: None,
            },
        )])
    }

    #[cfg(test)]
    pub(crate) fn record_installation(
        &mut self,
        tool_id: &str,
        version: &str,
        archive: Option<ArchiveState>,
    ) -> Result<()> {
        self.record_all(&[(
            tool_id.to_owned(),
            StateRecord::Installation {
                version: version.to_owned(),
                archive,
            },
        )])
    }

    #[cfg(test)]
    pub(crate) fn record_archive(&mut self, tool_id: &str, archive: ArchiveState) -> Result<()> {
        self.record_all(&[(tool_id.to_owned(), StateRecord::Archive(archive))])
    }

    /// Persists several tool records with one serialization and one atomic
    /// write, rolling every in-memory entry back when persistence fails.
    pub fn record_all(&mut self, entries: &[(String, StateRecord)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system time is before Unix epoch")?
            .as_secs();
        let previous = entries
            .iter()
            .map(|(tool_id, record)| -> Result<(String, Option<ToolState>)> {
                let before = self.data.tools.get(tool_id).cloned();
                match record {
                    StateRecord::Installation { version, archive } => {
                        self.data.tools.insert(
                            tool_id.clone(),
                            ToolState {
                                version: version.clone(),
                                updated_at,
                                archive: archive.clone(),
                            },
                        );
                    }
                    StateRecord::Archive(archive) => {
                        let entry = self
                            .data
                            .tools
                            .get_mut(tool_id)
                            .with_context(|| format!("cannot find state for tool {tool_id}"))?;
                        entry.archive = Some(archive.clone());
                    }
                }
                Ok((tool_id.clone(), before))
            })
            .collect::<Result<Vec<_>>>()?;
        // 持久化失败时恢复内存中的旧值，避免重试看到从未写入磁盘的更新。
        if let Err(error) = self.save() {
            for (tool_id, previous) in previous {
                if let Some(previous) = previous {
                    self.data.tools.insert(tool_id, previous);
                } else {
                    self.data.tools.remove(&tool_id);
                }
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
    use std::fs;

    use tempfile::tempdir;

    use super::{ArchiveState, StateRecord, StateStore};

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

    #[test]
    fn record_all_persists_every_entry_in_one_write() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.yaml");
        let mut state = StateStore::load(&path).unwrap();

        state
            .record_all(&[
                (
                    "bat".to_owned(),
                    StateRecord::Installation {
                        version: "v1.2.3".to_owned(),
                        archive: None,
                    },
                ),
                (
                    "frida".to_owned(),
                    StateRecord::Installation {
                        version: "16.7.19".to_owned(),
                        archive: None,
                    },
                ),
            ])
            .unwrap();

        let reloaded = StateStore::load(&path).unwrap();
        assert_eq!(reloaded.version("bat"), Some("v1.2.3"));
        assert_eq!(reloaded.version("frida"), Some("16.7.19"));
    }

    #[test]
    fn record_all_matches_sequential_record_results() {
        let batched = tempdir().unwrap();
        let sequential = tempdir().unwrap();
        let mut batched = StateStore::load(batched.path().join("state.yaml")).unwrap();
        let mut sequential = StateStore::load(sequential.path().join("state.yaml")).unwrap();
        let versions = ["v1.2.3", "16.7.19", "v1.3.0"];
        let entries = ["bat", "frida", "bat"]
            .into_iter()
            .zip(versions)
            .map(|(tool_id, version)| {
                (
                    tool_id.to_owned(),
                    StateRecord::Installation {
                        version: version.to_owned(),
                        archive: None,
                    },
                )
            })
            .collect::<Vec<_>>();

        batched.record_all(&entries).unwrap();
        for (tool_id, record) in &entries {
            let StateRecord::Installation { version, .. } = record else {
                unreachable!("the fixture only uses installations");
            };
            sequential.record(tool_id, version).unwrap();
        }

        assert_eq!(batched.version("bat"), sequential.version("bat"));
        assert_eq!(batched.version("frida"), sequential.version("frida"));
    }

    #[test]
    fn restores_in_memory_state_when_batched_persistence_fails() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("state");
        let path = parent.join("state.yaml");
        let mut state = StateStore::load(&path).unwrap();
        std::fs::write(&parent, "blocks directory creation").unwrap();

        let installation = |tool_id: &str, version: &str| {
            (
                tool_id.to_owned(),
                StateRecord::Installation {
                    version: version.to_owned(),
                    archive: None,
                },
            )
        };
        assert!(
            state
                .record_all(&[installation("alpha", "v1"), installation("beta", "v2")])
                .is_err()
        );
        assert_eq!(state.version("alpha"), None);
        assert_eq!(state.version("beta"), None);

        std::fs::remove_file(&parent).unwrap();
        state.record("working", "v3").unwrap();
        let reloaded = StateStore::load(&path).unwrap();
        assert_eq!(reloaded.version("alpha"), None);
        assert_eq!(reloaded.version("beta"), None);
        assert_eq!(reloaded.version("working"), Some("v3"));
    }

    #[test]
    fn persists_the_verified_archive_identity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.yaml");
        let archive = directory.path().join("Demo.7z");
        fs::write(&archive, "archive").unwrap();
        let identity = ArchiveState::capture(&archive).unwrap();
        let mut state = StateStore::load(&path).unwrap();
        state
            .record_installation("demo", "v1", Some(identity))
            .unwrap();

        let reloaded = StateStore::load(&path).unwrap();
        assert!(reloaded.archive("demo").unwrap().matches(&archive));
        fs::write(&archive, "changed archive").unwrap();
        assert!(!reloaded.archive("demo").unwrap().matches(&archive));
    }

    #[test]
    fn adds_an_archive_identity_without_changing_the_recorded_version() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.yaml");
        let archive = directory.path().join("Demo.7z");
        fs::write(&archive, "archive").unwrap();
        let mut state = StateStore::load(&path).unwrap();
        state.record("demo", "v1").unwrap();
        state
            .record_archive("demo", ArchiveState::capture(&archive).unwrap())
            .unwrap();

        let reloaded = StateStore::load(path).unwrap();
        assert_eq!(reloaded.version("demo"), Some("v1"));
        assert!(reloaded.archive("demo").unwrap().matches(&archive));
    }
}
