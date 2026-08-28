use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::Tool;
use crate::error::UpdaterError;
use crate::paths::installation_backup_path;

use super::super::paths::paths_overlap;

#[derive(Default)]
pub(super) struct ManagedPaths {
    entries: Vec<ManagedPath>,
}

impl ManagedPaths {
    pub(super) fn register(&mut self, config_path: &Path, tool: &Tool) -> Result<()> {
        let candidates = tool_paths(tool);
        for (index, candidate) in candidates.iter().enumerate() {
            for existing in candidates.iter().skip(index + 1) {
                if paths_conflict(candidate, existing) {
                    return Err(conflict_error(config_path, candidate, existing).into());
                }
            }
            if let Some(existing) = self
                .entries
                .iter()
                .find(|existing| paths_conflict(candidate, existing))
            {
                return Err(conflict_error(config_path, candidate, existing).into());
            }
        }
        self.entries.extend(candidates);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedPathKind {
    Destination,
    VersionMarker,
    SymlinkTarget,
    TransactionBackup,
}

impl ManagedPathKind {
    fn label(self) -> &'static str {
        match self {
            Self::Destination => "destination",
            Self::VersionMarker => "version marker",
            Self::SymlinkTarget => "symlink target",
            Self::TransactionBackup => "transaction backup",
        }
    }
}

struct ManagedPath {
    owner: String,
    path: PathBuf,
    kind: ManagedPathKind,
}

impl ManagedPath {
    fn new(tool: &Tool, path: PathBuf, kind: ManagedPathKind) -> Self {
        Self {
            owner: tool.id.clone(),
            path,
            kind,
        }
    }
}

fn tool_paths(tool: &Tool) -> Vec<ManagedPath> {
    let mut paths = Vec::with_capacity(4 + tool.install.symlinks.len());
    paths.push(ManagedPath::new(
        tool,
        tool.install.destination.clone(),
        ManagedPathKind::Destination,
    ));
    if let Some(backup) = installation_backup_path(&tool.install.destination) {
        paths.push(ManagedPath::new(
            tool,
            backup,
            ManagedPathKind::TransactionBackup,
        ));
    }
    if let Some(marker) = tool.version_marker_path() {
        paths.push(ManagedPath::new(
            tool,
            marker.clone(),
            ManagedPathKind::VersionMarker,
        ));
        if let Some(backup) = installation_backup_path(&marker) {
            paths.push(ManagedPath::new(
                tool,
                backup,
                ManagedPathKind::TransactionBackup,
            ));
        }
    }
    paths.extend(
        tool.install
            .symlinks
            .iter()
            .map(|link| ManagedPath::new(tool, link.to.clone(), ManagedPathKind::SymlinkTarget)),
    );
    paths
}

fn paths_conflict(left: &ManagedPath, right: &ManagedPath) -> bool {
    if !paths_overlap(&left.path, &right.path) {
        return false;
    }
    if left.owner != right.owner {
        return true;
    }
    if matches!(
        (left.kind, right.kind),
        (
            ManagedPathKind::Destination,
            ManagedPathKind::TransactionBackup
        ) | (
            ManagedPathKind::TransactionBackup,
            ManagedPathKind::Destination
        )
    ) {
        return false;
    }
    if left.kind == ManagedPathKind::TransactionBackup
        || right.kind == ManagedPathKind::TransactionBackup
    {
        return true;
    }
    matches!(
        (left.kind, right.kind),
        (
            ManagedPathKind::VersionMarker,
            ManagedPathKind::SymlinkTarget
        ) | (
            ManagedPathKind::SymlinkTarget,
            ManagedPathKind::VersionMarker
        ) | (
            ManagedPathKind::SymlinkTarget,
            ManagedPathKind::SymlinkTarget
        )
    )
}

fn conflict_error(path: &Path, left: &ManagedPath, right: &ManagedPath) -> UpdaterError {
    let message = if left.kind == ManagedPathKind::Destination
        && right.kind == ManagedPathKind::Destination
    {
        format!(
            "tools {} and {} have overlapping destinations {} and {}",
            right.owner,
            left.owner,
            right.path.display(),
            left.path.display()
        )
    } else if left.kind == ManagedPathKind::SymlinkTarget
        && right.kind == ManagedPathKind::SymlinkTarget
        && left.owner != right.owner
    {
        format!(
            "tools {} and {} share symlink target {}",
            right.owner,
            left.owner,
            left.path.display()
        )
    } else if left.owner == right.owner {
        format!(
            "tool {} {} {} conflicts with its {} {}",
            left.owner,
            left.kind.label(),
            left.path.display(),
            right.kind.label(),
            right.path.display()
        )
    } else {
        format!(
            "tool {} {} {} conflicts with tool {} {} {}",
            left.owner,
            left.kind.label(),
            left.path.display(),
            right.owner,
            right.kind.label(),
            right.path.display()
        )
    };
    UpdaterError::config(path, message)
}
