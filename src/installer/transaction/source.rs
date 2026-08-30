use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::{Builder, TempDir};

use crate::domain::{Tool, VERSION_FILE};

use super::super::filesystem::copy_tree;

pub(in crate::installer) struct CommitSource {
    pub(in crate::installer) ready: PathBuf,
    pub(in crate::installer) external_version: Option<PathBuf>,
    pub(in crate::installer) _adjacent_transaction: Option<TempDir>,
}

impl CommitSource {
    pub(in crate::installer) fn direct(ready: &Path, external_version: Option<&Path>) -> Self {
        Self {
            ready: ready.to_path_buf(),
            external_version: external_version.map(Path::to_path_buf),
            _adjacent_transaction: None,
        }
    }

    pub(in crate::installer) fn copy_next_to_destination(
        tool: &Tool,
        ready: &Path,
        external_version: Option<&Path>,
        parent: &Path,
    ) -> Result<Self> {
        let transaction = Builder::new()
            .prefix(&format!(".{}-commit-", tool.id))
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "cannot create cross-filesystem commit directory in {}",
                    parent.display()
                )
            })?;
        let copied_ready = transaction.path().join("ready");
        // Cross-filesystem by construction, so hard links could only fail
        // per file here; copy directly.
        copy_tree(ready, &copied_ready, false).with_context(|| {
            format!(
                "cannot transfer staged installation for {} from {} to {}",
                tool.id,
                ready.display(),
                copied_ready.display()
            )
        })?;
        let copied_version = external_version
            .map(|source| -> Result<PathBuf> {
                let destination = transaction.path().join(VERSION_FILE);
                fs::copy(source, &destination).with_context(|| {
                    format!(
                        "cannot transfer staged version marker for {} from {} to {}",
                        tool.id,
                        source.display(),
                        destination.display()
                    )
                })?;
                Ok(destination)
            })
            .transpose()?;
        Ok(Self {
            ready: copied_ready,
            external_version: copied_version,
            _adjacent_transaction: Some(transaction),
        })
    }
}

#[cfg(unix)]
pub(in crate::installer) fn same_filesystem(source: &Path, destination: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let source = fs::metadata(source)
        .with_context(|| format!("cannot inspect staging directory {}", source.display()))?;
    let destination = fs::metadata(destination).with_context(|| {
        format!(
            "cannot inspect destination parent {}",
            destination.display()
        )
    })?;
    Ok(source.dev() == destination.dev())
}

#[cfg(not(unix))]
pub(in crate::installer) fn same_filesystem(source: &Path, destination: &Path) -> Result<bool> {
    let probe = Builder::new()
        .prefix(".utu-filesystem-probe-")
        .tempfile_in(source)
        .with_context(|| format!("cannot create filesystem probe in {}", source.display()))?;
    let probe_path = probe.into_temp_path();
    let target = Builder::new()
        .prefix(".utu-filesystem-probe-")
        .tempfile_in(destination)
        .with_context(|| {
            format!(
                "cannot create filesystem probe target in {}",
                destination.display()
            )
        })?;
    let target_path = target.path().to_path_buf();
    target.close().with_context(|| {
        format!(
            "cannot prepare filesystem probe target {}",
            target_path.display()
        )
    })?;
    match fs::hard_link(&probe_path, &target_path) {
        Ok(()) => {
            fs::remove_file(&target_path).with_context(|| {
                format!(
                    "cannot remove filesystem probe target {}",
                    target_path.display()
                )
            })?;
            Ok(true)
        }
        Err(error) => {
            tracing::debug!(
                source = %source.display(),
                destination = %destination.display(),
                error = %error,
                "filesystem hard-link probe failed; using copy fallback"
            );
            Ok(false)
        }
    }
}
