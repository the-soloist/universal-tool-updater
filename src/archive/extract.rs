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

use super::{ExtractionContext, ExtractionLimits};

fn archive_error(path: &Path, message: impl Display) -> UpdaterError {
    UpdaterError::Archive {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

const IO_BUFFER_BYTES: usize = 256 * 1024;

/// 跨条目累计的解压配额：声明大小在解压前预扣以便快速失败，
/// 实际写出的字节数记在另一本账上，谎报头部的归档同样会撞到上限。
struct Quota {
    limits: ExtractionLimits,
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

    fn charge(
        &mut self,
        archive: &Path,
        entry: &str,
        size: u64,
    ) -> std::result::Result<(), UpdaterError> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(archive_error(
                archive,
                format!(
                    "{}extraction quota exceeded at entry {entry:?}: entry count {} exceeds max_entries {}",
                    self.tool_prefix, self.entries, self.limits.max_entries
                ),
            ));
        }
        self.charge_bytes(archive, entry, size)
    }

    fn charge_bytes(
        &mut self,
        archive: &Path,
        entry: &str,
        size: u64,
    ) -> std::result::Result<(), UpdaterError> {
        self.declared_bytes = self.declared_bytes.saturating_add(size);
        if self.declared_bytes > self.limits.max_total_bytes {
            return Err(archive_error(
                archive,
                format!(
                    "{}extraction quota exceeded at entry {entry:?}: cumulative uncompressed size {} bytes exceeds max_total_bytes {} bytes",
                    self.tool_prefix, self.declared_bytes, self.limits.max_total_bytes
                ),
            ));
        }
        Ok(())
    }

    /// 对实际写入磁盘的字节数单独计费，独立于声明大小的账本。
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

    /// 输出侧账本还能接受的字节数。
    fn remaining_actual(&self) -> u64 {
        self.limits
            .max_total_bytes
            .saturating_sub(self.actual_bytes)
    }

    fn tool_prefix(&self) -> &str {
        &self.tool_prefix
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

/// 解压输出侧的写入守卫：统计实际交给底层 sink 的字节数，
/// 剩余配额耗尽后拒绝继续写入，让超限载荷在写盘中途立即中断。
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

/// 7z 后端自管输出时的读取侧守卫：解压字节流经时计数，
/// 剩余配额耗尽后停止向后端供数。
struct CountingReader<'a> {
    inner: &'a mut dyn Read,
    remaining: u64,
    counted: u64,
    tripped: bool,
    /// 配额耗尽后探测读出的字节，等下一次 read 归还，避免丢弃数据。
    pending: Option<u8>,
}

impl<'a> CountingReader<'a> {
    fn new(inner: &'a mut dyn Read, remaining: u64) -> Self {
        Self {
            inner,
            remaining,
            counted: 0,
            tripped: false,
            pending: None,
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
        if let Some(byte) = self.pending.take() {
            buffer[0] = byte;
            return Ok(1);
        }
        if self.remaining == 0 {
            // 累计输出恰好等于配额是合法归档：后端会再读一次确认 EOF，
            // 此处先对底层做单字节探测，真 EOF 返回 Ok(0)，读到数据才判超额。
            let mut probe = [0_u8; 1];
            let probed = self.inner.read(&mut probe)?;
            if probed == 0 {
                return Ok(0);
            }
            self.counted += probed as u64;
            self.pending = Some(probe[0]);
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
        let (compression, encrypted, size, name) = {
            let entry = archive
                .by_index_raw(index)
                .map_err(|error| zip_error(archive_path, error))?;
            (
                entry.compression(),
                entry.encrypted(),
                entry.size(),
                entry.name().to_owned(),
            )
        };
        quota.charge(archive_path, &name, size)?;
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
            // CheckedZipWriter::finish 会 flush，此后读取 written 与落盘字节数一致。
            let mut limited = CountingWriter::new(file, quota.remaining_actual());
            let extracted =
                extract_zip_lzma(&mut entry, &mut limited, unpacked_size, crc32, archive_path);
            quota.charge_actual(archive_path, limited.written())?;
            if limited.tripped() {
                return Err(quota.actual_violation(archive_path).into());
            }
            extracted?;
            #[cfg(unix)]
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
        // copy_buffered 返回前会 flush BufWriter，此后读取 written 与落盘字节数一致。
        let mut limited = CountingWriter::new(File::create(&output)?, quota.remaining_actual());
        let copied = copy_buffered(&mut entry, &mut limited);
        quota.charge_actual(archive_path, limited.written())?;
        if limited.tripped() {
            return Err(quota.actual_violation(archive_path).into());
        }
        copied?;
        #[cfg(unix)]
        set_zip_permissions(&output, &entry)?;
    }
    Ok(())
}

fn zip_entry_output(
    archive: &Path,
    destination: &Path,
    entry: &ZipFile<'_>,
) -> Result<std::path::PathBuf> {
    // 先校验归档路径再拼接，避免目录穿越条目逃出解压目录。
    let enclosed = entry.enclosed_name().ok_or_else(|| UpdaterError::Archive {
        path: archive.to_path_buf(),
        message: format!("unsafe ZIP entry {:?}", entry.name()),
    })?;
    Ok(destination.join(enclosed))
}

fn zip_error(archive: &Path, error: zip::result::ZipError) -> UpdaterError {
    UpdaterError::Archive {
        path: archive.to_path_buf(),
        message: error.to_string(),
    }
}

fn extract_zip_lzma<R: Read, W: Write>(
    mut input: R,
    output: W,
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
            inner: BufWriter::with_capacity(IO_BUFFER_BYTES, inner),
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

#[cfg(unix)]
fn set_zip_permissions(output: &Path, entry: &ZipFile<'_>) -> Result<()> {
    if let Some(mode) = entry.unix_mode() {
        use std::os::unix::fs::PermissionsExt;
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
        if let Err(error) = quota.charge(archive, &entry.name, entry.size) {
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
    // 库级输出上限在每个成员解压前压到剩余配额：未声明大小或声明不可信的
    // 成员会在写盘中途被库拦截，落盘后的 charge_actual 仅作复核。
    let mut rar_limits = unrar_rs::Limits::default();

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
        quota.charge(
            archive,
            &member.info.name,
            member.info.unpacked_size.unwrap_or(0),
        )?;
        let relative = safe_rar_member_path(&member.info.name).ok_or_else(|| {
            archive_error(
                archive,
                format!("unsafe RAR entry {:?}", member.info.raw_name),
            )
        })?;
        let output = destination.join(relative);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        rar_limits.max_unpacked_size = rar_member_output_limit(
            quota.remaining_actual(),
            member.info.unpacked_size,
            member.info.compression.method,
        );
        reader.set_limits(rar_limits.clone());
        let written = reader
            .extract_member_to_file(member.index, &options, None, &output)
            .map_err(|error| {
                // 库侧 ResourceLimit 拦截就是配额违规，与其他配额错误一样带工具归属。
                let tool_prefix = if matches!(error, unrar_rs::RarError::ResourceLimit { .. }) {
                    quota.tool_prefix()
                } else {
                    ""
                };
                archive_error(
                    archive,
                    format!(
                        "{tool_prefix}RAR extraction failed for {:?}: {error}",
                        member.info.name
                    ),
                )
            })?;
        quota.charge_actual(archive, written)?;
    }
    Ok(())
}

fn rar_member_output_limit(
    remaining: u64,
    unpacked_size: Option<u64>,
    compression: unrar_rs::CompressionMethod,
) -> u64 {
    // unrar-rs 0.5.5 treats `written >= max_unpacked_size` as an overflow for
    // compressed members whose size is unknown. Reserve one probe byte so an
    // output exactly equal to our inclusive quota remains valid; Quota still
    // rejects any successful extraction whose actual output exceeds remaining.
    if unpacked_size.is_none() && compression != unrar_rs::CompressionMethod::Store {
        remaining.saturating_add(1)
    } else {
        remaining
    }
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

fn buffered_archive(archive: &Path) -> io::Result<BufReader<File>> {
    Ok(BufReader::with_capacity(
        IO_BUFFER_BYTES,
        File::open(archive)?,
    ))
}

fn copy_buffered<R: Read, W: Write>(input: &mut R, output: W) -> io::Result<u64> {
    let mut output = BufWriter::with_capacity(IO_BUFFER_BYTES, output);
    let copied = io::copy(input, &mut output)?;
    output.flush()?;
    Ok(copied)
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
    let mut quota = Quota::new(context);
    for entry in entries {
        let mut entry = entry.map_err(|error| archive_error(archive_path, error))?;
        // Entry::size includes PAX overrides and GNU sparse logical size;
        // Header::size only exposes the raw header value and can undercount.
        let size = entry.size();
        let entry_name = entry
            .path()
            .map_err(|error| archive_error(archive_path, error))?
            .display()
            .to_string();
        quota.charge(archive_path, &entry_name, size)?;
        let entry_type = entry.header().entry_type();
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
            // tar 头部声明的 size 决定读取流长度，落盘后再按实际字节数复核。
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

fn extract_single<R: io::Read>(
    mut reader: R,
    archive: &Path,
    destination: &Path,
    context: &ExtractionContext<'_>,
) -> Result<()> {
    let filename = archive
        .file_stem()
        .ok_or_else(|| archive_error(archive, "archive has no filename"))?;
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

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};

    use super::{CountingReader, IO_BUFFER_BYTES, copy_buffered, rar_member_output_limit};

    #[derive(Default)]
    struct CountingWriter {
        bytes: usize,
        calls: usize,
    }

    struct ChunkedReader<'a> {
        remaining: &'a [u8],
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.remaining.len().min(buffer.len()).min(8 * 1024);
            buffer[..read].copy_from_slice(&self.remaining[..read]);
            self.remaining = &self.remaining[read..];
            Ok(read)
        }
    }

    impl Write for CountingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes += buffer.len();
            self.calls += 1;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_buffer_coalesces_small_copy_writes() {
        let payload = vec![0_u8; IO_BUFFER_BYTES * 2 + 1];
        let mut input = ChunkedReader {
            remaining: &payload,
        };
        let mut output = CountingWriter::default();

        let copied = copy_buffered(&mut input, &mut output).unwrap();

        assert_eq!(copied, payload.len() as u64);
        assert_eq!(output.bytes, payload.len());
        assert_eq!(output.calls, 3);
    }

    #[test]
    fn counting_reader_accepts_output_equal_to_the_quota() {
        let payload = vec![1_u8; 4];
        let mut underlying = ChunkedReader {
            remaining: &payload,
        };
        let mut limited = CountingReader::new(&mut underlying, 4);

        let copied = io::copy(&mut limited, &mut io::sink()).unwrap();

        assert_eq!(copied, 4);
        assert_eq!(limited.counted(), 4);
        assert!(!limited.tripped());
    }

    #[test]
    fn counting_reader_trips_after_the_quota_when_more_data_remains() {
        let payload = vec![1_u8; 5];
        let mut underlying = ChunkedReader {
            remaining: &payload,
        };
        let mut limited = CountingReader::new(&mut underlying, 4);

        let error = io::copy(&mut limited, &mut io::sink()).unwrap_err();

        assert_eq!(error.to_string(), "extraction output quota exceeded");
        assert_eq!(limited.counted(), 5);
        assert!(limited.tripped());
    }

    #[test]
    fn counting_reader_returns_the_probed_byte_on_the_next_read() {
        let payload = vec![7_u8; 5];
        let mut underlying = ChunkedReader {
            remaining: &payload,
        };
        let mut limited = CountingReader::new(&mut underlying, 4);

        let mut buffer = [0_u8; 4];
        assert_eq!(limited.read(&mut buffer).unwrap(), 4);
        assert!(limited.read(&mut buffer).is_err());

        // 探测读出的字节不能丢弃：错误之后如果后端仍读取，须原样归还。
        let mut recovered = [0_u8; 1];
        assert_eq!(limited.read(&mut recovered).unwrap(), 1);
        assert_eq!(recovered[0], 7);
    }

    #[test]
    fn rar_output_limit_keeps_exact_unknown_compressed_members_legal() {
        use unrar_rs::CompressionMethod;

        assert_eq!(
            rar_member_output_limit(4, None, CompressionMethod::Normal),
            5
        );
        assert_eq!(
            rar_member_output_limit(4, None, CompressionMethod::Store),
            4
        );
        assert_eq!(
            rar_member_output_limit(4, Some(4), CompressionMethod::Normal),
            4
        );
    }
}
