use std::fs;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::error::UpdaterError;
use crate::paths::safe_filename;

/// Schema version 2 adds the prefix digest fields; older metadata without
/// them fails to deserialize and is discarded once.
pub(super) const SCHEMA_VERSION: u32 = 2;

/// A lock file older than this is treated as abandoned regardless of the pid
/// it names, covering pid reuse and unavailable liveness checks.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct Metadata {
    pub(super) schema_version: u32,
    pub(super) filename: String,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) total: Option<u64>,
    /// SHA-256 of the first `prefix_len` bytes of the partial file as of the
    /// last metadata save; the next session re-hashes the file and compares
    /// before resuming, so locally tampered partials are discarded.
    pub(super) prefix_sha256: String,
    pub(super) prefix_len: u64,
    /// Marks a transport-verified complete cache entry; only trustworthy when
    /// `verified` is `Transport` and `prefix_len` equals `total`, so a
    /// metadata write interrupted mid-download cannot masquerade as complete.
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

/// Result of validating a cached partial download for resumption: the checked
/// metadata, the byte offset to resume from and a hasher covering exactly
/// those bytes.
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

fn cache_key(url: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(url.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn paths(directory: &Path, url: &str) -> (PathBuf, PathBuf) {
    let key = cache_key(url);
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
    // The cache filename is the URL fingerprint; recompute it from the current
    // URL so metadata saved for a different URL is discarded.
    let expected_name = format!("{}.yaml", cache_key(url));
    match metadata {
        Ok(metadata)
            if metadata.schema_version == SCHEMA_VERSION
                && path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name == expected_name)
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

/// Validates the cached partial against its recorded prefix digest and
/// prepares the resume state. The prefix is re-hashed and compared before
/// anything is trusted, so tampered, truncated or mismatched caches are
/// discarded. Bytes written past the last persisted checkpoint are truncated
/// so the HTTP range and the digest resume from the same offset.
pub(super) fn prepare_resume(
    metadata_path: &Path,
    partial: &Path,
    metadata: Option<Metadata>,
) -> Result<ResumeState> {
    let Some(metadata) = metadata else {
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
    let checkpoint_length = metadata.prefix_len;
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
    // 文件可能先写入字节、后持久化对应检查点；截去尾部未检查点数据，确保哈希和 HTTP 范围从同一偏移恢复。
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

    let hasher = hash_prefix_state(partial, checkpoint_length)?;
    if !hex_digest(&hasher.clone().finalize()).eq_ignore_ascii_case(&metadata.prefix_sha256) {
        return discard_corrupt_cache(
            metadata_path,
            partial,
            "cached file SHA-256 does not match its checkpoint",
        );
    }
    // 只有最终长度检查点和传输校验都成立时才能信任完成标记，避免写元数据中断后误用半成品缓存。
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
    Ok(ResumeState {
        metadata: Some(metadata),
        downloaded: checkpoint_length,
        hasher,
    })
}

/// Persists the in-flight prefix digest as the new checkpoint and returns the
/// hex digest. `complete` marks a fully transferred and transport-verified
/// body, making the cache reusable across runs until installation succeeds.
pub(super) fn checkpoint(
    path: &Path,
    metadata: &mut Metadata,
    prefix_len: u64,
    hasher: &Sha256,
    complete: bool,
) -> Result<String> {
    let digest = hex_digest(&hasher.clone().finalize());
    metadata.schema_version = SCHEMA_VERSION;
    metadata.prefix_len = prefix_len;
    metadata.prefix_sha256 = digest.clone();
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

/// Hashes the first `len` bytes of `partial` and returns the live hasher
/// state; errors when the file is shorter.
pub(super) fn hash_prefix_state(partial: &Path, len: u64) -> Result<Sha256> {
    let file = fs::File::open(partial)
        .with_context(|| format!("cannot open partial download {}", partial.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("cannot hash partial download {}", partial.display()))?;
        if read == 0 {
            anyhow::bail!(
                "partial download {} is shorter than {len} bytes",
                partial.display()
            );
        }
        let take = (read as u64).min(remaining) as usize;
        hasher.update(&buffer[..take]);
        remaining -= take as u64;
    }
    Ok(hasher)
}

/// SHA-256 hex digest of the first `len` bytes of `partial`; errors when the
/// file is shorter.
pub(super) fn hash_prefix(partial: &Path, len: u64) -> Result<String> {
    Ok(hex_digest(&hash_prefix_state(partial, len)?.finalize()))
}

/// SHA-256 of the empty byte string; the prefix digest of a fresh download.
pub(super) fn empty_prefix_sha256() -> String {
    hex_digest(&Sha256::new().finalize())
}

fn hex_digest(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Exclusive per-tool session lock over the partial directory. Two updater
/// instances appending to the same `.part` would corrupt it, so a second
/// instance refuses the tool until the holder exits.
#[derive(Debug)]
pub(super) struct SessionLock {
    path: PathBuf,
}

impl SessionLock {
    pub(super) fn acquire(directory: &Path, tool_id: &str) -> Result<Self> {
        let path = directory.join("lock");
        match create_lock(&path) {
            Ok(()) => Ok(Self { path }),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if lock_conflicts(&path)? {
                    return Err(UpdaterError::Download {
                        tool: tool_id.to_owned(),
                        message: "another updater instance is updating this tool".to_owned(),
                    }
                    .into());
                }
                // Dead or over-aged holder: clear the stale lock and take over.
                fs::remove_file(&path).with_context(|| {
                    format!("cannot remove stale partial lock {}", path.display())
                })?;
                create_lock(&path)
                    .with_context(|| format!("cannot create partial lock {}", path.display()))?;
                Ok(Self { path })
            }
            Err(error) => {
                Err(error).with_context(|| format!("cannot create partial lock {}", path.display()))
            }
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            tracing::warn!(
                lock = %self.path.display(),
                error = %error,
                "cannot remove the partial download session lock"
            );
        }
    }
}

fn create_lock(path: &Path) -> std::result::Result<(), std::io::Error> {
    let pid = std::process::id();
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(format!("{pid}\n{started}\n").as_bytes())?;
    file.sync_all()
}

/// A lock held by a live process conflicts; an over-aged lock never does.
/// When the liveness check is inconclusive the lock is treated as held.
fn lock_conflicts(path: &Path) -> Result<bool> {
    if modified_age(path)? >= LOCK_STALE_AFTER {
        return Ok(false);
    }
    let pid = fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().next().map(str::trim).map(ToOwned::to_owned))
        .and_then(|line| line.parse::<u32>().ok());
    match pid.and_then(process_is_alive) {
        Some(alive) => Ok(alive),
        None => Ok(true),
    }
}

fn modified_age(path: &Path) -> Result<Duration> {
    let modified = fs::metadata(path)
        .with_context(|| format!("cannot inspect partial lock {}", path.display()))?
        .modified()
        .with_context(|| {
            format!(
                "cannot read the modification time of partial lock {}",
                path.display()
            )
        })?;
    SystemTime::now().duration_since(modified).map_err(|_| {
        anyhow::anyhow!(
            "partial lock {} has a modification time in the future",
            path.display()
        )
    })
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> Option<bool> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .ok()?;
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(
        text.split_whitespace()
            .any(|token| token == pid.to_string()),
    )
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> Option<bool> {
    let status = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .ok()?;
    if status.success() {
        return Some(true);
    }
    // kill also fails with EPERM for live processes owned by others; /proc
    // distinguishes that from a dead pid where it is available.
    match std::fs::metadata(format!("/proc/{pid}")) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
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
        Metadata, SCHEMA_VERSION, Verification, hash_prefix, load, paths, prepare_resume, save,
    };

    #[cfg(unix)]
    use super::empty_prefix_sha256;

    fn sha256(contents: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        let digest = hasher.finalize();
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn metadata_for(partial: &std::path::Path, contents: &[u8]) -> Metadata {
        Metadata {
            schema_version: SCHEMA_VERSION,
            filename: "tool.zip".to_owned(),
            etag: Some("\"fixture-v1\"".to_owned()),
            last_modified: None,
            total: None,
            prefix_sha256: hash_prefix(partial, contents.len() as u64).unwrap(),
            prefix_len: contents.len() as u64,
            complete: false,
            verified: Verification::None,
        }
    }

    #[test]
    fn discards_a_cache_when_its_sha256_checkpoint_does_not_match() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = paths(directory.path(), "https://example.test/tool.zip");
        fs::write(&partial, "damaged").unwrap();
        let mut metadata = metadata_for(&partial, b"damaged");
        metadata.prefix_sha256 = "0".repeat(64);
        save(&metadata_path, &metadata).unwrap();

        let loaded = load(&metadata_path, &partial, "https://example.test/tool.zip").unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();

        assert!(resume.metadata.is_none());
        assert_eq!(resume.downloaded, 0);
        assert!(!metadata_path.exists());
        assert!(!partial.exists());
    }

    #[test]
    fn truncates_bytes_after_the_last_sha256_checkpoint() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = paths(directory.path(), "https://example.test/tool.zip");
        fs::write(&partial, "hello uncheckpointed tail").unwrap();
        let mut metadata = metadata_for(&partial, b"hello");
        metadata.total = Some(64);
        save(&metadata_path, &metadata).unwrap();

        let loaded = load(&metadata_path, &partial, "https://example.test/tool.zip").unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();

        assert_eq!(resume.downloaded, 5);
        assert_eq!(fs::read(partial).unwrap(), b"hello");
    }

    #[test]
    fn refuses_a_completed_cache_that_was_never_transport_verified() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = paths(directory.path(), "https://example.test/tool.zip");
        fs::write(&partial, "hello").unwrap();
        let mut metadata = metadata_for(&partial, b"hello");
        metadata.total = Some(5);
        metadata.complete = true;
        save(&metadata_path, &metadata).unwrap();

        let loaded = load(&metadata_path, &partial, "https://example.test/tool.zip").unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();

        assert!(resume.metadata.is_none());
        assert!(!metadata_path.exists());
        assert!(!partial.exists());
    }

    #[test]
    fn accepts_a_transport_verified_completed_cache() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = paths(directory.path(), "https://example.test/tool.zip");
        fs::write(&partial, "hello").unwrap();
        let mut metadata = metadata_for(&partial, b"hello");
        metadata.total = Some(5);
        metadata.complete = true;
        metadata.verified = Verification::Transport;
        save(&metadata_path, &metadata).unwrap();

        let loaded = load(&metadata_path, &partial, "https://example.test/tool.zip").unwrap();
        let resume = prepare_resume(&metadata_path, &partial, loaded).unwrap();

        assert_eq!(resume.downloaded, 5);
        assert!(resume.metadata.is_some());
        assert_eq!(sha256(b"hello").len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn discards_symlinked_partial_files_without_touching_their_target() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = paths(directory.path(), "https://example.test/tool.zip");
        let target = directory.path().join("target");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &partial).unwrap();
        save(
            &metadata_path,
            &Metadata {
                schema_version: SCHEMA_VERSION,
                filename: "tool.zip".to_owned(),
                etag: None,
                last_modified: None,
                total: Some(4),
                prefix_sha256: empty_prefix_sha256(),
                prefix_len: 0,
                complete: false,
                verified: Verification::None,
            },
        )
        .unwrap();

        assert!(
            load(&metadata_path, &partial, "https://example.test/tool.zip")
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
        assert!(!metadata_path.exists());
        assert!(!partial.exists());
    }
}

#[cfg(test)]
mod integrity_tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        Metadata, SCHEMA_VERSION, SessionLock, Verification, hash_prefix, load, paths,
        prepare_resume, save,
    };

    fn cache_for(
        directory: &std::path::Path,
        url: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        paths(directory, url)
    }

    fn metadata_for(partial: &std::path::Path, contents: &[u8]) -> Metadata {
        Metadata {
            schema_version: SCHEMA_VERSION,
            filename: "artifact.bin".to_owned(),
            etag: Some("\"resume-v1\"".to_owned()),
            last_modified: None,
            total: Some(11),
            prefix_sha256: hash_prefix(partial, contents.len() as u64).unwrap(),
            prefix_len: contents.len() as u64,
            complete: false,
            verified: Verification::None,
        }
    }

    fn resume(directory: &std::path::Path, url: &str) -> super::ResumeState {
        let (partial, metadata_path) = cache_for(directory, url);
        let loaded = load(&metadata_path, &partial, url).unwrap();
        prepare_resume(&metadata_path, &partial, loaded).unwrap()
    }

    #[test]
    fn rejects_a_tampered_partial_and_discards_it() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = cache_for(directory.path(), "https://example.test/a");
        let original = b"hello ";
        fs::write(&partial, original).unwrap();
        save(&metadata_path, &metadata_for(&partial, original)).unwrap();
        // Flip one byte without changing the length.
        let mut contents = original.to_vec();
        contents[2] ^= 0x01;
        fs::write(&partial, &contents).unwrap();

        let state = resume(directory.path(), "https://example.test/a");
        assert!(
            state.metadata.is_none(),
            "a tampered partial must not be resumed"
        );
        assert!(!partial.exists());
        assert!(!metadata_path.exists());
    }

    #[test]
    fn rejects_a_truncated_partial_and_discards_it() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = cache_for(directory.path(), "https://example.test/a");
        let original = b"hello ";
        fs::write(&partial, original).unwrap();
        save(&metadata_path, &metadata_for(&partial, original)).unwrap();
        fs::write(&partial, &original[..4]).unwrap();

        let state = resume(directory.path(), "https://example.test/a");
        assert!(state.metadata.is_none());
        assert!(!partial.exists());
        assert!(!metadata_path.exists());
    }

    #[test]
    fn discards_prefixless_metadata_from_the_previous_schema() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = cache_for(directory.path(), "https://example.test/a");
        fs::write(&partial, b"hello ").unwrap();
        fs::write(
            &metadata_path,
            "schema_version: 1\nfilename: artifact.bin\netag: '\"resume-v1\"'\nlast_modified: null\ntotal: 11\n",
        )
        .unwrap();

        assert!(
            load(&metadata_path, &partial, "https://example.test/a")
                .unwrap()
                .is_none()
        );
        assert!(!partial.exists());
        assert!(!metadata_path.exists());
    }

    #[test]
    fn accepts_an_intact_partial() {
        let directory = tempdir().unwrap();
        let (partial, metadata_path) = cache_for(directory.path(), "https://example.test/a");
        fs::write(&partial, b"hello ").unwrap();
        save(&metadata_path, &metadata_for(&partial, b"hello ")).unwrap();

        let state = resume(directory.path(), "https://example.test/a");
        let loaded = state.metadata.expect("an intact partial must load");
        assert_eq!(loaded.prefix_len, 6);
        assert_eq!(state.downloaded, 6);
    }

    #[test]
    fn a_lock_held_by_a_live_process_conflicts() {
        let directory = tempdir().unwrap();
        let lock = directory.path().join("lock");
        // The test process itself is guaranteed to be alive.
        fs::write(&lock, format!("{}\n0\n", std::process::id())).unwrap();

        let error = SessionLock::acquire(directory.path(), "demo").unwrap_err();
        assert!(
            error.to_string().contains("another updater instance"),
            "expected a conflict, got {error:#}"
        );
        assert!(lock.exists());
    }

    #[test]
    fn a_lock_from_a_dead_process_is_taken_over() {
        let directory = tempdir().unwrap();
        let lock = directory.path().join("lock");
        // A pid this high is never allocated in practice.
        fs::write(&lock, "3999999996\n0\n").unwrap();

        let session = SessionLock::acquire(directory.path(), "demo").unwrap();
        assert!(lock.exists(), "the takeover must write a fresh lock");
        drop(session);
        assert!(!lock.exists(), "the session lock must be released on drop");
    }

    #[test]
    fn an_overaged_lock_is_taken_over_regardless_of_its_pid() {
        let directory = tempdir().unwrap();
        let lock = directory.path().join("lock");
        fs::write(&lock, format!("{}\n0\n", std::process::id())).unwrap();
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 60 * 60);
        let file = std::fs::File::options().write(true).open(&lock).unwrap();
        file.set_modified(stale).unwrap();
        drop(file);

        let session = SessionLock::acquire(directory.path(), "demo").unwrap();
        drop(session);
        assert!(!lock.exists());
    }
}
