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

use super::ExtractionContext;

const IO_BUFFER_BYTES: usize = 256 * 1024;

fn archive_error(path: &Path, message: impl Display) -> UpdaterError {
    UpdaterError::Archive {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

pub(super) fn zip(archive_path: &Path, destination: &Path, password: Option<&str>) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(BufReader::with_capacity(IO_BUFFER_BYTES, file))
        .map_err(|error| UpdaterError::Archive {
            path: archive_path.to_path_buf(),
            message: error.to_string(),
        })?;
    for index in 0..archive.len() {
        let (compression, encrypted) = {
            let entry = archive
                .by_index_raw(index)
                .map_err(|error| zip_error(archive_path, error))?;
            (entry.compression(), entry.encrypted())
        };
        if compression == CompressionMethod::Lzma {
            if encrypted {
                return Err(UpdaterError::Archive {
                    path: archive_path.to_path_buf(),
                    message: "encrypted ZIP LZMA entries are not supported".to_owned(),
                }
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
            extract_zip_lzma(&mut entry, file, unpacked_size, crc32, archive_path)?;
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
        copy_buffered(&mut entry, File::create(&output)?)?;
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

pub(super) fn seven_zip(archive: &Path, destination: &Path, password: Option<&str>) -> Result<()> {
    let result = if let Some(password) = password {
        sevenz_rust::decompress_file_with_password(archive, destination, password.into())
    } else {
        sevenz_rust::decompress_file(archive, destination)
    };
    result.map_err(|error| {
        UpdaterError::Archive {
            path: archive.to_path_buf(),
            message: format!("7z extraction failed: {error}"),
        }
        .into()
    })
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
    .map_err(|error| UpdaterError::Archive {
        path: archive.to_path_buf(),
        message: format!("RAR open failed: {error}"),
    })?;
    let options = ExtractOptions {
        verify: true,
        password: password.map(ToOwned::to_owned),
        restore_owners: false,
    };
    let destination_root = destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());

    for member in reader.indexed_member_infos() {
        if !member.extractable {
            return Err(UpdaterError::Archive {
                path: archive.to_path_buf(),
                message: format!(
                    "RAR member {:?} requires missing volumes {:?}",
                    member.info.name, member.missing_volumes
                ),
            }
            .into());
        }
        let relative =
            safe_rar_member_path(&member.info.name).ok_or_else(|| UpdaterError::Archive {
                path: archive.to_path_buf(),
                message: format!("unsafe RAR entry {:?}", member.info.raw_name),
            })?;
        // unrar-rs 已把 UnixSymlink/WindowsSymlink/WindowsJunction 归一为 is_symlink。
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
        reader
            .extract_member_to_file(member.index, &options, None, &output)
            .map_err(|error| UpdaterError::Archive {
                path: archive.to_path_buf(),
                message: format!("RAR extraction failed for {:?}: {error}", member.info.name),
            })?;
    }
    Ok(())
}

/// RAR 链接成员默认拒绝，与 tar 策略一致。opt-in 路径复用 tar 侧的
/// 界内校验；RAR3 的 symlink 目标位于成员载荷、头部无目标，保持拒绝。
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
    let tool_prefix = link_tool_prefix(context);
    if !context.allow_symlinks {
        return Err(archive_error(
            archive_path,
            format!(
                "{tool_prefix}RAR member {member:?} is a {kind}; set install.allow_symlinks_in_archive to allow links"
            ),
        )
        .into());
    }
    let target = target.ok_or_else(|| {
        archive_error(
            archive_path,
            format!("{tool_prefix}RAR member {member:?} is a {kind} without a target"),
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

pub(super) fn gzip(archive: &Path, destination: &Path) -> Result<()> {
    extract_single(
        GzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
    )
}

pub(super) fn xz(archive: &Path, destination: &Path) -> Result<()> {
    extract_single(
        XzDecoder::new(buffered_archive(archive)?),
        archive,
        destination,
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
    let entries = archive.entries().map_err(|error| UpdaterError::Archive {
        path: archive_path.to_path_buf(),
        message: error.to_string(),
    })?;
    let destination_root = destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());
    for entry in entries {
        let mut entry = entry.map_err(|error| UpdaterError::Archive {
            path: archive_path.to_path_buf(),
            message: error.to_string(),
        })?;
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
        if entry_type.is_symlink() {
            unpack_tar_symlink(archive_path, destination, &mut entry)?;
            continue;
        }
        let unpacked = entry
            .unpack_in(destination)
            .map_err(|error| UpdaterError::Archive {
                path: archive_path.to_path_buf(),
                message: error.to_string(),
            })?;
        if !unpacked {
            return Err(UpdaterError::Archive {
                path: archive_path.to_path_buf(),
                message: "archive entry attempted to escape the destination".to_owned(),
            }
            .into());
        }
    }
    Ok(())
}

/// 自建 tar 符号链接：tar 0.4.46 在 Windows 对所有 symlink 固定
/// symlink_file，指向目录的链接无法跟随；这里按目标类型选择创建方式。
/// 目标尚未落盘时无法判定类型，按文件链接创建，因此 Windows 上的
/// 目录型悬空链接需要目标先存在才能正确建链。
fn unpack_tar_symlink<R: io::Read>(
    archive_path: &Path,
    destination: &Path,
    entry: &mut tar::Entry<'_, R>,
) -> Result<()> {
    let path = entry
        .path()
        .map_err(|error| archive_error(archive_path, error))?;
    let target = entry
        .link_name()
        .map_err(|error| archive_error(archive_path, error))?
        .ok_or_else(|| {
            archive_error(
                archive_path,
                format!("symbolic link {path:?} without a target"),
            )
        })?;
    // 与 unpack_in 的逃逸判定对齐：含绝对分量或 `..` 的条目拒绝解压。
    if path.components().any(|component| {
        matches!(
            component,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir
        )
    }) {
        return Err(archive_error(
            archive_path,
            "archive entry attempted to escape the destination",
        )
        .into());
    }
    let link = destination.join(path.as_os_str());
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link)?;
    #[cfg(windows)]
    {
        // NT 路径解析只把反斜杠当分隔符，正斜杠的 tar 目标会让链接无法跟随。
        let target = PathBuf::from(target.to_string_lossy().replace('/', "\\"));
        let resolved = link.parent().unwrap_or(destination).join(&target);
        let target_is_directory = fs::metadata(&resolved).is_ok_and(|metadata| metadata.is_dir());
        if target_is_directory {
            std::os::windows::fs::symlink_dir(&target, &link)?;
        } else {
            std::os::windows::fs::symlink_file(&target, &link)?;
        }
    }
    Ok(())
}

/// 符号链接与硬链接默认拒绝，与自更新链路的安全基线一致。
/// opt-in 路径仍要求链接目标为相对路径且解析后位于解压目录内。
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
    let tool_prefix = link_tool_prefix(context);
    if !context.allow_symlinks {
        return Err(archive_error(
            archive_path,
            format!(
                "{tool_prefix}archive entry {path:?} is a {kind}; set install.allow_symlinks_in_archive to allow links"
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
                format!("{tool_prefix}archive entry {path:?} is a {kind} without a target"),
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

/// 链接目标必须是相对路径，且与条目位置拼接后仍位于解压目录内。
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
            // 目标可能尚未落盘：按词法归一化后对照两个形式的解压根目录。
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

/// 词法归一化 `.` 与 `..` 分量，使尚未物化的链接目标也能对照解压根目录检查。
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

/// 链接拒绝错误中的工具归属前缀；非工具作用域解压时为空。
fn link_tool_prefix(context: &ExtractionContext<'_>) -> String {
    context
        .tool_id
        .map(|id| format!("tool {id}: "))
        .unwrap_or_default()
}

fn extract_single<R: io::Read>(mut reader: R, archive: &Path, destination: &Path) -> Result<()> {
    let filename = archive.file_stem().ok_or_else(|| UpdaterError::Archive {
        path: archive.to_path_buf(),
        message: "archive has no filename".to_owned(),
    })?;
    let mut output = File::create(destination.join(filename))?;
    io::copy(&mut reader, &mut output)?;
    Ok(())
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

    use super::{IO_BUFFER_BYTES, copy_buffered};

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
}
