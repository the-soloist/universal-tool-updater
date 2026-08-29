use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::{Digest as Sha2Digest, Sha256};
use tempfile::NamedTempFile;

use crate::paths::safe_filename;

const LEGACY_SCHEMA_VERSION: u32 = 1;
pub(super) const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct Metadata {
    pub(super) schema_version: u32,
    pub(super) url: String,
    pub(super) filename: String,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) total: Option<u64>,
    #[serde(default)]
    pub(super) downloaded: Option<u64>,
    #[serde(default)]
    pub(super) sha256: Option<String>,
    #[serde(default)]
    pub(super) complete: bool,
    #[serde(default)]
    pub(super) verified: Verification,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Verification {
    #[default]
    None,
    Transport,
}

pub(super) struct ResumeState {
    pub(super) metadata: Option<Metadata>,
    pub(super) downloaded: u64,
    pub(super) hasher: Sha256,
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
            if matches!(
                metadata.schema_version,
                LEGACY_SCHEMA_VERSION | SCHEMA_VERSION
            ) && metadata.url == url
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

pub(super) fn prepare_resume(
    metadata_path: &Path,
    partial: &Path,
    metadata: Option<Metadata>,
) -> Result<ResumeState> {
    let Some(mut metadata) = metadata else {
        return Ok(empty_resume());
    };
    let file_length = fs::metadata(partial)
        .with_context(|| format!("cannot inspect partial download {}", partial.display()))?
        .len();
    if metadata.total.is_some_and(|total| file_length > total) {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "cached file is larger than the recorded remote total",
        );
    }

    if metadata.schema_version == LEGACY_SCHEMA_VERSION {
        let hasher = hash_file(partial, file_length)?;
        checkpoint(metadata_path, &mut metadata, file_length, &hasher, false)?;
        tracing::debug!(
            path = %metadata_path.display(),
            bytes = file_length,
            "migrated partial download metadata to schema v2"
        );
        return Ok(ResumeState {
            metadata: Some(metadata),
            downloaded: file_length,
            hasher,
        });
    }

    let Some(checkpoint_length) = metadata.downloaded else {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "schema v2 metadata has no downloaded checkpoint",
        );
    };
    let Some(expected_digest) = metadata
        .sha256
        .as_deref()
        .filter(|value| valid_sha256(value))
    else {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "schema v2 metadata has no valid SHA-256 checkpoint",
        );
    };
    if checkpoint_length > file_length
        || metadata
            .total
            .is_some_and(|total| checkpoint_length > total)
    {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "SHA-256 checkpoint length is inconsistent with the cached file",
        );
    }
    if metadata.complete
        && (metadata.verified != Verification::Transport
            || metadata
                .total
                .is_some_and(|total| checkpoint_length != total))
    {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "completed cache metadata is not transport-verified",
        );
    }
    if !metadata.complete && metadata.verified != Verification::None {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "incomplete cache metadata has an invalid verification state",
        );
    }

    if file_length > checkpoint_length {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(partial)
            .with_context(|| format!("cannot open partial download {}", partial.display()))?;
        file.set_len(checkpoint_length).with_context(|| {
            format!(
                "cannot truncate uncheckpointed download tail in {}",
                partial.display()
            )
        })?;
        file.sync_all()
            .with_context(|| format!("cannot sync partial download {}", partial.display()))?;
        tracing::warn!(
            path = %partial.display(),
            discarded_bytes = file_length - checkpoint_length,
            "discarded an uncheckpointed download tail after interruption"
        );
    }

    let hasher = hash_file(partial, checkpoint_length)?;
    let actual_digest = digest(&hasher);
    if !actual_digest.eq_ignore_ascii_case(expected_digest) {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "cached file SHA-256 does not match its checkpoint",
        );
    }
    Ok(ResumeState {
        metadata: Some(metadata),
        downloaded: checkpoint_length,
        hasher,
    })
}

pub(super) fn checkpoint(
    path: &Path,
    metadata: &mut Metadata,
    downloaded: u64,
    hasher: &Sha256,
    complete: bool,
) -> Result<String> {
    let digest = digest(hasher);
    metadata.schema_version = SCHEMA_VERSION;
    metadata.downloaded = Some(downloaded);
    metadata.sha256 = Some(digest.clone());
    metadata.complete = complete;
    metadata.verified = if complete {
        Verification::Transport
    } else {
        Verification::None
    };
    save(path, metadata)?;
    Ok(digest)
}

fn empty_resume() -> ResumeState {
    ResumeState {
        metadata: None,
        downloaded: 0,
        hasher: Sha256::new(),
    }
}

fn hash_file(path: &Path, expected_length: u64) -> Result<Sha256> {
    let mut input = fs::File::open(path)
        .with_context(|| format!("cannot open partial download {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut hashed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .with_context(|| format!("cannot hash partial download {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
    }
    if hashed != expected_length {
        anyhow::bail!(
            "partial download {} changed while hashing: expected {expected_length} bytes, read {hashed}",
            path.display()
        );
    }
    Ok(hasher)
}

pub(super) fn digest(hasher: &Sha256) -> String {
    let digest = hasher.clone().finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn discard_corrupt_cache(
    metadata_path: &Path,
    partial: &Path,
    reason: &str,
) -> Result<ResumeState> {
    tracing::warn!(
        metadata = %metadata_path.display(),
        partial = %partial.display(),
        reason,
        "discarding corrupt partial download cache"
    );
    clear(metadata_path, partial)?;
    Ok(empty_resume())
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

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        LEGACY_SCHEMA_VERSION, Metadata, SCHEMA_VERSION, Verification, digest, load,
        prepare_resume, save,
    };

    fn sha256(contents: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        digest(&hasher)
    }

    fn metadata(schema_version: u32, url: &str, filename: &str) -> Metadata {
        Metadata {
            schema_version,
            url: url.to_owned(),
            filename: filename.to_owned(),
            etag: Some("\"fixture-v1\"".to_owned()),
            last_modified: None,
            total: None,
            downloaded: None,
            sha256: None,
            complete: false,
            verified: Verification::None,
        }
    }

    #[test]
    fn migrates_schema_v1_metadata_with_a_sha256_checkpoint() {
        let directory = tempdir().unwrap();
        let metadata_path = directory.path().join("download.yaml");
        let partial = directory.path().join("download.part");
        let url = "https://example.test/tool.zip";
        let contents = b"hello";
        fs::write(&partial, contents).unwrap();
        save(
            &metadata_path,
            &metadata(LEGACY_SCHEMA_VERSION, url, "tool.zip"),
        )
        .unwrap();

        let loaded = load(&metadata_path, &partial, url).unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();
        let migrated = resume.metadata.unwrap();

        assert_eq!(resume.downloaded, contents.len() as u64);
        assert_eq!(migrated.schema_version, SCHEMA_VERSION);
        assert_eq!(migrated.downloaded, Some(contents.len() as u64));
        assert_eq!(
            migrated.sha256.as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        assert!(!migrated.complete);
        assert_eq!(migrated.verified, Verification::None);

        let persisted: Metadata =
            yaml_serde::from_str(&fs::read_to_string(metadata_path).unwrap()).unwrap();
        assert_eq!(persisted.sha256, migrated.sha256);
    }

    #[test]
    fn discards_a_cache_when_its_sha256_checkpoint_does_not_match() {
        let directory = tempdir().unwrap();
        let metadata_path = directory.path().join("download.yaml");
        let partial = directory.path().join("download.part");
        let url = "https://example.test/tool.zip";
        fs::write(&partial, "damaged").unwrap();
        let mut metadata = metadata(SCHEMA_VERSION, url, "tool.zip");
        metadata.downloaded = Some(7);
        metadata.sha256 = Some("0".repeat(64));
        save(&metadata_path, &metadata).unwrap();

        let loaded = load(&metadata_path, &partial, url).unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();

        assert!(resume.metadata.is_none());
        assert_eq!(resume.downloaded, 0);
        assert!(!metadata_path.exists());
        assert!(!partial.exists());
    }

    #[test]
    fn truncates_bytes_after_the_last_sha256_checkpoint() {
        let directory = tempdir().unwrap();
        let metadata_path = directory.path().join("download.yaml");
        let partial = directory.path().join("download.part");
        let url = "https://example.test/tool.zip";
        fs::write(&partial, "hello uncheckpointed tail").unwrap();
        let mut metadata = metadata(SCHEMA_VERSION, url, "tool.zip");
        metadata.total = Some(64);
        metadata.downloaded = Some(5);
        metadata.sha256 = Some(sha256(b"hello"));
        save(&metadata_path, &metadata).unwrap();

        let loaded = load(&metadata_path, &partial, url).unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();

        assert_eq!(resume.downloaded, 5);
        assert_eq!(fs::read(partial).unwrap(), b"hello");
    }

    #[cfg(unix)]
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
                downloaded: Some(4),
                sha256: Some(
                    "1dcc98a76ba3f46e8d5e287e3c1f21b3bda5b9895498444a931ca58c6917edc6".to_owned(),
                ),
                complete: true,
                verified: super::Verification::Transport,
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
