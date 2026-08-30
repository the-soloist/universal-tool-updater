use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::domain::{ResolvedArtifact, Tool};
use crate::paths::safe_filename;
use crate::workspace::ToolWorkspace;

use super::completion::{materialize_file, replace_file};
use super::partial::{Metadata as PartialMetadata, SCHEMA_VERSION, Verification, hash_prefix};

pub(super) fn recover_previous_download(
    tool: &Tool,
    version: &str,
    artifact: &ResolvedArtifact,
    workspace: &ToolWorkspace,
    partial: &Path,
    current_length: u64,
) -> Result<Option<(PartialMetadata, u64)>> {
    let Some(filename) = artifact.filename.as_deref().and_then(safe_filename) else {
        return Ok(None);
    };
    let normalized_version = version.strip_prefix('v').unwrap_or(version);
    // 完成下载会在运行之间共享，只有文件名或 URL 能证明属于当前版本时才复用，避免旧产物冒充新更新。
    if normalized_version.is_empty()
        || (!filename.contains(version)
            && !filename.contains(normalized_version)
            && !artifact.url.contains(version)
            && !artifact.url.contains(normalized_version))
    {
        return Ok(None);
    }
    let Some(previous) = workspace.recoverable_download(&filename)? else {
        return Ok(None);
    };
    let previous_length = fs::metadata(&previous)
        .with_context(|| format!("cannot inspect previous download {}", previous.display()))?
        .len();
    if previous_length <= current_length {
        return Ok(None);
    }

    if current_length == 0 {
        materialize_file(&previous, partial)
    } else {
        replace_file(&previous, partial)
    }
    .with_context(|| {
        format!(
            "cannot recover completed download {} for {}",
            previous.display(),
            tool.id
        )
    })?;
    // The recovered bytes were never hashed in this run, so the prefix
    // digest is computed now; the next load re-hashes and compares as usual.
    let prefix_sha256 = hash_prefix(partial, previous_length)?;
    let metadata = PartialMetadata {
        schema_version: SCHEMA_VERSION,
        filename,
        etag: None,
        last_modified: None,
        total: Some(previous_length),
        prefix_sha256,
        prefix_len: previous_length,
        complete: false,
        verified: Verification::None,
    };
    tracing::debug!(
        tool = %tool.id,
        filename = %metadata.filename,
        bytes = previous_length,
        replaced_bytes = current_length,
        source = %previous.display(),
        "recovered completed artifact from an interrupted run"
    );
    Ok(Some((metadata, previous_length)))
}
