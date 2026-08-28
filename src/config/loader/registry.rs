mod managed_paths;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::Tool;
use crate::error::UpdaterError;

use managed_paths::ManagedPaths;

#[derive(Default)]
pub(super) struct ToolRegistry {
    tools: BTreeMap<String, Tool>,
    profiles: BTreeMap<String, PathBuf>,
    managed_paths: ManagedPaths,
}

impl ToolRegistry {
    pub(super) fn add_profile(
        &mut self,
        manifest: &Path,
        path: &Path,
        profile: &str,
    ) -> Result<()> {
        if let Some(previous) = self.profiles.insert(profile.to_owned(), path.to_path_buf()) {
            return Err(UpdaterError::config(
                manifest,
                format!(
                    "include files {} and {} define the same profile {profile}",
                    previous.display(),
                    path.display()
                ),
            )
            .into());
        }
        Ok(())
    }

    pub(super) fn ensure_unique_id(&self, path: &Path, id: &str) -> Result<()> {
        if self.tools.contains_key(id) {
            return Err(UpdaterError::config(path, format!("duplicate tool id {id}")).into());
        }
        Ok(())
    }

    pub(super) fn insert(&mut self, path: &Path, tool: Tool) -> Result<()> {
        self.managed_paths.register(path, &tool)?;
        self.tools.insert(tool.id.clone(), tool);
        Ok(())
    }

    pub(super) fn finish(self) -> BTreeMap<String, Tool> {
        self.tools
    }
}
