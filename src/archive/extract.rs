use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path};

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

pub(super) fn zip(archive_path: &Path, destination: &Path, password: Option<&str>) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive =
        zip::ZipArchive::new(BufReader::new(file)).map_err(|error| UpdaterError::Archive {
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
        let mut file = File::create(&output)?;
        io::copy(&mut entry, &mut file)?;
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
        .decompress(&mut BufReader::new(input), &mut output)
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

pub(super) fn rar(archive: &Path, destination: &Path, password: Option<&str>) -> Result<()> {
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

pub(super) fn tar_gzip(archive: &Path, destination: &Path) -> Result<()> {
    extract_tar(GzDecoder::new(File::open(archive)?), archive, destination)
}

pub(super) fn tar_bzip2(archive: &Path, destination: &Path) -> Result<()> {
    extract_tar(BzDecoder::new(File::open(archive)?), archive, destination)
}

pub(super) fn tar_xz(archive: &Path, destination: &Path) -> Result<()> {
    extract_tar(XzDecoder::new(File::open(archive)?), archive, destination)
}

pub(super) fn gzip(archive: &Path, destination: &Path) -> Result<()> {
    extract_single(GzDecoder::new(File::open(archive)?), archive, destination)
}

pub(super) fn xz(archive: &Path, destination: &Path) -> Result<()> {
    extract_single(XzDecoder::new(File::open(archive)?), archive, destination)
}

fn extract_tar<R: io::Read>(reader: R, archive_path: &Path, destination: &Path) -> Result<()> {
    let mut archive = TarArchive::new(reader);
    let entries = archive.entries().map_err(|error| UpdaterError::Archive {
        path: archive_path.to_path_buf(),
        message: error.to_string(),
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|error| UpdaterError::Archive {
            path: archive_path.to_path_buf(),
            message: error.to_string(),
        })?;
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
