use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tempfile::NamedTempFile;

use crate::paths::safe_filename;

pub(super) const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct Metadata {
    pub(super) schema_version: u32,
    pub(super) url: String,
    pub(super) filename: String,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) total: Option<u64>,
}

impl Metadata {
    pub(super) fn validator(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|etag| !etag.trim_start().starts_with("W/"))
            .or(self.last_modified.as_deref())
    }
}

pub(super) fn paths(directory: &Path, url: &str) -> (PathBuf, PathBuf) {
    let mut digest = Sha1::new();
    digest.update(url.as_bytes());
    let key = format!("{:x}", digest.finalize());
    (
        directory.join(format!("{key}.part")),
        directory.join(format!("{key}.yaml")),
    )
}

pub(super) fn load(path: &Path, partial: &Path, url: &str) -> Result<Option<Metadata>> {
    let metadata_file = regular_file(path)?;
    let partial_file = regular_file(partial)?;
    if metadata_file != Some(true) || partial_file != Some(true) {
        if metadata_file == Some(false) || partial_file == Some(false) {
            tracing::warn!(
                metadata = %path.display(),
                partial = %partial.display(),
                "discarding non-regular partial download cache"
            );
        }
        clear(path, partial)?;
        return Ok(None);
    }
    let input = fs::read_to_string(path)
        .with_context(|| format!("cannot read partial download metadata {}", path.display()))?;
    let metadata = yaml_serde::from_str::<Metadata>(&input);
    match metadata {
        Ok(metadata)
            if metadata.schema_version == SCHEMA_VERSION
                && metadata.url == url
                && safe_filename(&metadata.filename).as_deref()
                    == Some(metadata.filename.as_str()) =>
        {
            Ok(Some(metadata))
        }
        Ok(_) | Err(_) => {
            tracing::warn!(path = %path.display(), "discarding invalid partial download metadata");
            clear(path, partial)?;
            Ok(None)
        }
    }
}

fn regular_file(path: &Path) -> Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type().is_file())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect partial download cache {}", path.display())),
    }
}

pub(super) fn save(path: &Path, metadata: &Metadata) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "cannot create partial metadata directory {}",
            parent.display()
        )
    })?;
    let encoded = yaml_serde::to_string(metadata).context("cannot encode partial metadata")?;
    let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "cannot create partial metadata file in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(encoded.as_bytes())
        .context("cannot write partial metadata")?;
    temporary
        .as_file()
        .sync_all()
        .context("cannot sync partial metadata")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace partial metadata {}", path.display()))?;
    Ok(())
}

pub(super) fn length(metadata: Option<&Metadata>, partial: &Path) -> Result<u64> {
    if metadata.is_none() {
        return Ok(0);
    }
    fs::metadata(partial)
        .map(|value| value.len())
        .with_context(|| format!("cannot inspect partial download {}", partial.display()))
}

pub(super) fn clear(metadata: &Path, partial: &Path) -> Result<()> {
    for path in [metadata, partial] {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != ErrorKind::NotFound
        {
            return Err(error)
                .with_context(|| format!("cannot remove partial download {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{Metadata, SCHEMA_VERSION, load, save};

    #[test]
    fn discards_symlinked_partial_files_without_touching_their_target() {
        let directory = tempdir().unwrap();
        let metadata = directory.path().join("download.yaml");
        let partial = directory.path().join("download.part");
        let target = directory.path().join("target");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &partial).unwrap();
        save(
            &metadata,
            &Metadata {
                schema_version: SCHEMA_VERSION,
                url: "https://example.test/tool.zip".to_owned(),
                filename: "tool.zip".to_owned(),
                etag: None,
                last_modified: None,
                total: Some(4),
            },
        )
        .unwrap();

        assert!(
            load(&metadata, &partial, "https://example.test/tool.zip")
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
        assert!(!metadata.exists());
        assert!(!partial.exists());
    }
}
