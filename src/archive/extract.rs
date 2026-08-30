use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use bzip2::read::BzDecoder;
use crc_fast::{CrcAlgorithm, Digest as CrcDigest};
use flate2::read::GzDecoder;
use lzma_rs::decompress::raw::{LzmaDecoder, LzmaParams, LzmaProperties};
use tar::Archive as TarArchive;
use unrar_rs::{ExtractOptions, RarArchive};
use xz2::read::XzDecoder;
use zip::{CompressionMethod, read::ZipFile};

use crate::error::UpdaterError;
use crate::paths::{is_portable_component, is_portable_filename};

use super::{ExtractionContext, Limits};

fn archive_error(path: &Path, message: impl Display) -> UpdaterError {
    UpdaterError::Archive {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

/// I/O buffer for extraction streams: 256 KiB sits at the knee of the
/// syscall-reduction curve (D1), beyond which each doubling buys < 7%.
const IO_BUFFER_BYTES: usize = 256 * 1024;

/// Cumulative extraction quota tracked across archive entries. Declared
/// header sizes are charged up front for fast failure, while bytes actually
/// written out are counted on a separate ledger so archives lying about
/// their entry sizes still hit the same ceiling.
struct Quota {
    limits: Limits,
    tool_prefix: String,
    declared_bytes: u64,
    actual_bytes: u64,
    entries: usize,
}

impl Quota {
    fn new(context: &ExtractionContext<'_>) -> Self {
        Self {
            limits: context.limits,
            tool_prefix: context
                .tool_id
                .map(|id| format!("tool {id}: "))
                .unwrap_or_default(),
            declared_bytes: 0,
            actual_bytes: 0,
            entries: 0,
        }
    }

    fn charge(&mut self, archive: &Path, size: u64) -> std::result::Result<(), UpdaterError> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(archive_error(
                archive,
                format!(
                    "{}extraction quota exceeded: entry count {} exceeds max_entries {}",
                    self.tool_prefix, self.entries, self.limits.max_entries
                ),
            ));
        }
        self.charge_bytes(archive, size)
    }

    fn charge_bytes(&mut self, archive: &Path, size: u64) -> std::result::Result<(), UpdaterError> {
        self.declared_bytes = self.declared_bytes.saturating_add(size);
        if self.declared_bytes > self.limits.max_total_bytes {
            return Err(archive_error(
                archive,
                format!(
                    "{}extraction quota exceeded: cumulative uncompressed size {} bytes exceeds max_total_bytes {} bytes",
                    self.tool_prefix, self.declared_bytes, self.limits.max_total_bytes
                ),
            ));
        }
        Ok(())
    }

    /// Charges bytes that were actually written to disk, independently of the
    /// declared-size ledger above.
    fn charge_actual(
        &mut self,
        archive: &Path,
        size: u64,
    ) -> std::result::Result<(), UpdaterError> {
        self.actual_bytes = self.actual_bytes.saturating_add(size);
        if self.actual_bytes > self.limits.max_total_bytes {
            return Err(self.actual_violation(archive));
        }
        Ok(())
    }

    /// Bytes the output-side ledger still accepts before hitting the ceiling.
    fn remaining_actual(&self) -> u64 {
        self.limits
            .max_total_bytes
            .saturating_sub(self.actual_bytes)
    }

    fn actual_violation(&self, archive: &Path) -> UpdaterError {
        archive_error(
            archive,
            format!(
                "{}extraction quota exceeded: extracted output of {} bytes exceeds max_total_bytes {} bytes",
                self.tool_prefix, self.actual_bytes, self.limits.max_total_bytes
            ),
        )
    }
}

/// Writer-side guard for extraction output: counts the bytes actually handed
/// to the sink and refuses further writes once the remaining quota is
/// exhausted, so oversized payloads abort mid-write instead of filling the
/// disk.
struct CountingWriter<W> {
    inner: W,
    remaining: u64,
    written: u64,
    tripped: bool,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W, remaining: u64) -> Self {
        Self {
            inner,
            remaining,
            written: 0,
            tripped: false,
        }
    }

    fn written(&self) -> u64 {
        self.written
    }

    fn tripped(&self) -> bool {
        self.tripped
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.tripped = true;
            return Err(io::Error::other("extraction output quota exceeded"));
        }
        let allowed = (buffer.len() as u64).min(self.remaining) as usize;
        let written = self.inner.write(&buffer[..allowed])?;
        self.remaining -= written as u64;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reader-side guard for extraction backends that own their output sink:
/// counts decompressed bytes as they stream through and stops feeding the
/// backend once the remaining quota is exhausted.
struct CountingReader<'a> {
    inner: &'a mut dyn Read,
    remaining: u64,
    counted: u64,
    tripped: bool,
}

impl<'a> CountingReader<'a> {
    fn new(inner: &'a mut dyn Read, remaining: u64) -> Self {
        Self {
            inner,
            remaining,
            counted: 0,
            tripped: false,
        }
    }

    fn counted(&self) -> u64 {
        self.counted
    }

    fn tripped(&self) -> bool {
        self.tripped
    }
}

impl Read for CountingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.tripped = true;
            return Err(io::Error::other("extraction output quota exceeded"));
        }
        let allowed = (buffer.len() as u64).min(self.remaining) as usize;
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining -= read as u64;
        self.counted += read as u64;
        Ok(read)
    }
}

pub(super) fn zip(
    archive_path: &Path,
    destination: &Path,
    password: Option<&str>,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(BufReader::with_capacity(IO_BUFFER_BYTES, file))
        .map_err(|error| zip_error(archive_path, error))?;
    let mut quota = Quota::new(context);
    for index in 0..archive.len() {
        let (compression, encrypted, size) = {
            let entry = archive
                .by_index_raw(index)
                .map_err(|error| zip_error(archive_path, error))?;
            (entry.compression(), entry.encrypted(), entry.size())
        };
        quota.charge(archive_path, size)?;
        if compression == CompressionMethod::Lzma {
            if encrypted {
                return Err(archive_error(
                    archive_path,
                    "encrypted ZIP LZMA entries are not supported",
                )
                .into());
            }
            let mut entry = archive
                .by_index_raw(index)
                .map_err(|error| zip_error(archive_path, error))?;
            let output = zip_entry_output(archive_path, destination, &entry)?;
            if entry.is_dir() {
                fs::create_dir_all(&output)?;
                continue;
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = File::create(&output)?;
            let unpacked_size = entry.size();
            let crc32 = entry.crc32();
            let mut limited = CountingWriter::new(
                BufWriter::with_capacity(IO_BUFFER_BYTES, file),
                quota.remaining_actual(),
            );
            let extracted =
                extract_zip_lzma(&mut entry, &mut limited, unpacked_size, crc32, archive_path);
            // Flush before reading written() so the output ledger and the
            // bytes on disk agree when the quota trips mid-write.
            let flushed = limited.flush();
            quota.charge_actual(archive_path, limited.written())?;
            let tripped = limited.tripped();
            if tripped {
                return Err(quota.actual_violation(archive_path).into());
            }
            extracted?;
            flushed?;
            set_zip_permissions(&output, &entry)?;
            continue;
        }
        let entry = if let Some(password) = password {
            archive.by_index_decrypt(index, password.as_bytes())
        } else {
            archive.by_index(index)
        };
        let mut entry = entry.map_err(|error| zip_error(archive_path, error))?;
        let output = zip_entry_output(archive_path, destination, &entry)?;
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(&output)?;
        let mut limited = CountingWriter::new(
            BufWriter::with_capacity(IO_BUFFER_BYTES, file),
            quota.remaining_actual(),
        );
        let copied = io::copy(&mut entry, &mut limited);
        // Flush before reading written() so the output ledger and the bytes
        // on disk agree when the quota trips mid-write.
        let flushed = limited.flush();
        quota.charge_actual(archive_path, limited.written())?;
        let tripped = limited.tripped();
        if tripped {
            return Err(quota.actual_violation(archive_path).into());
        }
        copied?;
        flushed?;
        set_zip_permissions(&output, &entry)?;
    }
    Ok(())
}

fn zip_entry_output(
    archive: &Path,
    destination: &Path,
    entry: &ZipFile<'_>,
) -> Result<std::path::PathBuf> {
    let enclosed = entry
        .enclosed_name()
        .ok_or_else(|| archive_error(archive, format!("unsafe ZIP entry {:?}", entry.name())))?;
    ensure_portable_entry(archive, "ZIP", &enclosed)?;
    Ok(destination.join(enclosed))
}

/// Rejects entry names whose components cannot round-trip portably on
/// Windows: NTFS alternate-data-stream colons, reserved device names
/// (CON/PRN/AUX/NUL/COM1-9/LPT1-9), illegal characters and trailing
/// dots/spaces. This mirrors the safe_filename policy used by the
/// resolver-side filename derivation.
fn ensure_portable_entry(
    archive: &Path,
    kind: &str,
    path: &Path,
) -> std::result::Result<(), UpdaterError> {
    let portable = path.components().all(|component| {
        matches!(component, Component::Normal(value)
            if value.to_str().is_some_and(|value| is_portable_component(value, false)))
    });
    if portable {
        Ok(())
    } else {
        Err(archive_error(
            archive,
            format!(
                "unsafe {kind} entry {path:?}: every component must be a portable file name (no ':', reserved device names, or trailing spaces/dots)"
            ),
        ))
    }
}

fn zip_error(archive: &Path, error: zip::result::ZipError) -> UpdaterError {
    archive_error(archive, error)
}

/// ZIP LZMA entries carry the raw LZMA1 property byte described in ZIP APP NOTE
/// 5.8.8: the byte packs the decoder parameters as d = (pb * 5 + lp) * 9 + lc,
/// so valid values stay below 9 * 5 * 5 = 225. Decoding additionally requires
/// lc + lp <= 4 and a dictionary of at least 4 KiB; smaller stored sizes are
/// raised to that floor.
fn extract_zip_lzma<R: Read>(
    mut input: R,
    output: &mut dyn Write,
    unpacked_size: u64,
    expected_crc32: u32,
    archive: &Path,
) -> Result<()> {
    // 字典大小来自归档文件，因此限制解码器的内存分配上限。
    const MAX_DICTIONARY_SIZE: u32 = 512 * 1024 * 1024;

    let mut header = [0_u8; 4];
    input
        .read_exact(&mut header)
        .map_err(|error| lzma_error(archive, format!("invalid ZIP LZMA header: {error}")))?;
    let property_size = u16::from_le_bytes([header[2], header[3]]);
    if property_size != 5 {
        return Err(lzma_error(
            archive,
            format!("unsupported ZIP LZMA property size {property_size}"),
        )
        .into());
    }
    let mut properties = [0_u8; 5];
    input
        .read_exact(&mut properties)
        .map_err(|error| lzma_error(archive, format!("truncated ZIP LZMA properties: {error}")))?;

    let mut packed = u32::from(properties[0]);
    if packed >= 225 {
        return Err(lzma_error(
            archive,
            format!("invalid ZIP LZMA property byte {}", properties[0]),
        )
        .into());
    }
    let lc = packed % 9;
    packed /= 9;
    let lp = packed % 5;
    let pb = packed / 5;
    if lc + lp > 4 {
        return Err(lzma_error(
            archive,
            format!("invalid ZIP LZMA literal properties lc={lc}, lp={lp}"),
        )
        .into());
    }

    let dictionary_size =
        u32::from_le_bytes(properties[1..5].try_into().expect("four bytes")).max(4 * 1024);
    if dictionary_size > MAX_DICTIONARY_SIZE {
        return Err(lzma_error(
            archive,
            format!("ZIP LZMA dictionary size {dictionary_size} exceeds the safety limit"),
        )
        .into());
    }
    let params = LzmaParams::new(
        LzmaProperties { lc, lp, pb },
        dictionary_size,
        Some(unpacked_size),
    );
    let mut decoder = LzmaDecoder::new(params, Some(MAX_DICTIONARY_SIZE as usize))
        .map_err(|error| lzma_error(archive, error.to_string()))?;
    let mut output = CheckedZipWriter::new(output);
    decoder
        .decompress(
            &mut BufReader::with_capacity(IO_BUFFER_BYTES, input),
            &mut output,
        )
        .map_err(|error| lzma_error(archive, error.to_string()))?;
    let (written, actual_crc32) = output
        .finish()
        .map_err(|error| lzma_error(archive, error.to_string()))?;
    if written != unpacked_size {
        return Err(lzma_error(
            archive,
            format!("decompressed {written} bytes, expected {unpacked_size}"),
        )
        .into());
    }
    if actual_crc32 != expected_crc32 {
        return Err(lzma_error(
            archive,
            format!("CRC-32 mismatch: expected {expected_crc32:08x}, got {actual_crc32:08x}"),
        )
        .into());
    }
    Ok(())
}

struct CheckedZipWriter<W: Write> {
    inner: std::io::BufWriter<W>,
    crc32: CrcDigest,
    written: u64,
}

impl<W: Write> CheckedZipWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: std::io::BufWriter::new(inner),
            crc32: CrcDigest::new(CrcAlgorithm::Crc32IsoHdlc),
            written: 0,
        }
    }

    fn finish(mut self) -> io::Result<(u64, u32)> {
        self.inner.flush()?;
        Ok((self.written, self.crc32.finalize() as u32))
    }
}

impl<W: Write> Write for CheckedZipWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.crc32.update(&buffer[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn lzma_error(archive: &Path, message: String) -> UpdaterError {
    UpdaterError::Archive {
        path: archive.to_path_buf(),
        message: format!("ZIP LZMA extraction failed: {message}"),
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn set_zip_permissions(output: &Path, entry: &ZipFile<'_>) -> Result<()> {
    #[cfg(unix)]
    if let Some(mode) = entry.unix_mode() {
        use std::os::unix::fs::PermissionsExt;
        // Whitelist mask: setuid/setgid/sticky bits never survive extraction.
        fs::set_permissions(output, fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}

pub(super) fn seven_zip(
    archive: &Path,
    destination: &Path,
    password: Option<&str>,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let mut quota = Quota::new(context);
    let mut violation = None;
    let extract = |entry: &sevenz_rust::ArchiveEntry,
                   reader: &mut dyn Read,
                   dest: &PathBuf|
     -> std::result::Result<bool, sevenz_rust::Error> {
        if let Err(error) = quota.charge(archive, entry.size) {
            violation = Some(error);
            return Err(sevenz_rust::Error::Other(
                "extraction quota exceeded".into(),
            ));
        }
        let mut limited = CountingReader::new(reader, quota.remaining_actual());
        let result = sevenz_rust::default_entry_extract_fn(entry, &mut limited, dest);
        if let Err(error) = quota.charge_actual(archive, limited.counted()) {
            violation = Some(error);
            return Err(sevenz_rust::Error::Other(
                "extraction quota exceeded".into(),
            ));
        }
        if limited.tripped() {
            violation = Some(quota.actual_violation(archive));
            return Err(sevenz_rust::Error::Other(
                "extraction quota exceeded".into(),
            ));
        }
        result
    };
    let result = if let Some(password) = password {
        sevenz_rust::decompress_with_extract_fn_and_password(
            File::open(archive)?,
            destination,
            password.into(),
            extract,
        )
    } else {
        sevenz_rust::decompress_file_with_extract_fn(archive, destination, extract)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => match violation {
            Some(error) => Err(error.into()),
            None => Err(archive_error(archive, format!("7z extraction failed: {error}")).into()),
        },
    }
}

pub(super) fn rar(
    archive: &Path,
    destination: &Path,
    password: Option<&str>,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let file = File::open(archive)?;
    let mut reader = if let Some(password) = password {
        RarArchive::open_with_password(file, password)
    } else {
        RarArchive::open(file)
    }
    .map_err(|error| archive_error(archive, format!("RAR open failed: {error}")))?;
    let options = ExtractOptions {
        verify: true,
        password: password.map(ToOwned::to_owned),
        restore_owners: false,
    };
    let mut quota = Quota::new(context);
    let destination_root = destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());

    for member in reader.indexed_member_infos() {
        if !member.extractable {
            return Err(archive_error(
                archive,
                format!(
                    "RAR member {:?} requires missing volumes {:?}",
                    member.info.name, member.missing_volumes
                ),
            )
            .into());
        }
        quota.charge(archive, member.info.unpacked_size.unwrap_or(0))?;
        let relative = safe_rar_member_path(&member.info.name).ok_or_else(|| {
            archive_error(
                archive,
                format!("unsafe RAR entry {:?}", member.info.raw_name),
            )
        })?;
        if member.info.is_symlink || member.info.is_hardlink {
            ensure_rar_link_is_allowed(
                archive,
                destination,
                &destination_root,
                relative,
                member.info.is_hardlink,
                member.info.link_target.as_deref(),
                context,
            )?;
        }
        let output = destination.join(relative);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let written = reader
            .extract_member_to_file(member.index, &options, None, &output)
            .map_err(|error| {
                archive_error(
                    archive,
                    format!("RAR extraction failed for {:?}: {error}", member.info.name),
                )
            })?;
        quota.charge_actual(archive, written)?;
    }
    Ok(())
}

/// RAR link members default to rejection, matching the tar policy. The opt-in
/// path reuses the tar-side containment rules; RAR3 symlinks carrying their
/// target in the member payload surface no header target and stay rejected.
fn ensure_rar_link_is_allowed(
    archive_path: &Path,
    destination: &Path,
    destination_root: &Path,
    member: &Path,
    is_hardlink: bool,
    target: Option<&str>,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let kind = if is_hardlink {
        "hard link"
    } else {
        "symbolic link"
    };
    if !context.allow_symlinks {
        return Err(archive_error(
            archive_path,
            format!(
                "RAR member {member:?} is a {kind}; set install.allow_symlinks_in_archive to allow links"
            ),
        )
        .into());
    }
    let target = target.ok_or_else(|| {
        archive_error(
            archive_path,
            format!("RAR member {member:?} is a {kind} without a target"),
        )
    })?;
    ensure_bounded_link_target(
        archive_path,
        destination,
        destination_root,
        kind,
        member,
        Path::new(target),
    )
}

pub(super) fn tar_gzip(
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    extract_tar(
        GzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
        context,
    )
}

pub(super) fn tar_bzip2(
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    extract_tar(
        BzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
        context,
    )
}

pub(super) fn tar_xz(
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    extract_tar(
        XzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
        context,
    )
}

pub(super) fn gzip(
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    extract_single(
        GzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
        context,
    )
}

pub(super) fn xz(
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    extract_single(
        XzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
        context,
    )
}

/// Wraps the archive in a 256 KiB reader so the streaming decoders pull
/// compressed data in large chunks instead of per-byte syscalls.
fn buffered_archive(archive: &Path) -> io::Result<BufReader<File>> {
    Ok(BufReader::with_capacity(
        IO_BUFFER_BYTES,
        File::open(archive)?,
    ))
}

fn extract_tar<R: io::Read>(
    reader: R,
    archive_path: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let mut archive = TarArchive::new(reader);
    let entries = archive
        .entries()
        .map_err(|error| archive_error(archive_path, error))?;
    let destination_root = destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());
    let mut quota = Quota::new(context);
    for entry in entries {
        let mut entry = entry.map_err(|error| archive_error(archive_path, error))?;
        let size = entry
            .header()
            .size()
            .map_err(|error| archive_error(archive_path, error))?;
        quota.charge(archive_path, size)?;
        let entry_path = entry
            .path()
            .map_err(|error| archive_error(archive_path, error))?;
        ensure_portable_entry(archive_path, "tar", entry_path.as_ref())?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            ensure_link_is_allowed(
                archive_path,
                destination,
                &destination_root,
                &mut entry,
                context,
            )?;
        }
        let unpacked = entry
            .unpack_in(destination)
            .map_err(|error| archive_error(archive_path, error))?;
        if !unpacked {
            return Err(archive_error(
                archive_path,
                "archive entry attempted to escape the destination",
            )
            .into());
        }
        if entry_type.is_file() {
            let written = entry
                .path()
                .ok()
                .and_then(|path| fs::metadata(destination.join(path.as_ref())).ok())
                .map(|metadata| metadata.len())
                .unwrap_or(size);
            quota.charge_actual(archive_path, written)?;
        }
    }
    Ok(())
}

/// Symbolic and hard links default to rejection, matching the self-update
/// baseline. The opt-in path still requires a relative target that resolves
/// back inside the extraction directory.
fn ensure_link_is_allowed<R: io::Read>(
    archive_path: &Path,
    destination: &Path,
    destination_root: &Path,
    entry: &mut tar::Entry<'_, R>,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let path = entry
        .path()
        .map_err(|error| archive_error(archive_path, error))?;
    let kind = if entry.header().entry_type().is_symlink() {
        "symbolic link"
    } else {
        "hard link"
    };
    if !context.allow_symlinks {
        return Err(archive_error(
            archive_path,
            format!(
                "archive entry {path:?} is a {kind}; set install.allow_symlinks_in_archive to allow links"
            ),
        )
        .into());
    }
    let target = entry
        .link_name()
        .map_err(|error| archive_error(archive_path, error))?
        .ok_or_else(|| {
            archive_error(
                archive_path,
                format!("archive entry {path:?} is a {kind} without a target"),
            )
        })?;
    ensure_bounded_link_target(
        archive_path,
        destination,
        destination_root,
        kind,
        path.as_ref(),
        target.as_ref(),
    )
}

/// Shared opt-in validation for archive links: the target must be relative
/// and resolve back inside the extraction directory.
fn ensure_bounded_link_target(
    archive_path: &Path,
    destination: &Path,
    destination_root: &Path,
    kind: &str,
    member: &Path,
    target: &Path,
) -> Result<()> {
    if target.is_absolute() {
        return Err(archive_error(
            archive_path,
            format!("{kind} {member:?} target {target:?} must be relative"),
        )
        .into());
    }
    let link_directory = destination
        .join(member)
        .parent()
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            archive_error(
                archive_path,
                format!("{kind} {member:?} has no parent directory"),
            )
        })?;
    let resolved = link_directory.join(target);
    let contained = match resolved.canonicalize() {
        Ok(real) => real.starts_with(destination_root),
        Err(_) => {
            let lexical = lexical_root(&resolved);
            lexical.starts_with(destination) || lexical.starts_with(destination_root)
        }
    };
    if !contained {
        return Err(archive_error(
            archive_path,
            format!("{kind} {member:?} target {target:?} escapes the extraction directory"),
        )
        .into());
    }
    Ok(())
}

/// Lexically normalizes `.` and `..` components so link targets that have not
/// been materialized yet can still be checked against the extraction root.
fn lexical_root(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn extract_single<R: io::Read>(
    mut reader: R,
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let filename = archive
        .file_stem()
        .ok_or_else(|| archive_error(archive, "archive has no filename"))?;
    let filename = Path::new(filename);
    if !is_portable_filename(filename) {
        return Err(archive_error(
            archive,
            format!(
                "archive file name {filename:?} is not a portable file name (reserved device names, illegal characters, or trailing spaces/dots)"
            ),
        )
        .into());
    }
    let mut quota = Quota::new(context);
    let mut output = File::create(destination.join(filename))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        quota.charge_actual(archive, read as u64)?;
        output.write_all(&buffer[..read])?;
    }
}

fn safe_rar_member_path(value: &str) -> Option<&Path> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        None
    } else {
        Some(path)
    }
}
