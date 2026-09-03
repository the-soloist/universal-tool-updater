mod extract;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sevenz_rust::encoder_options::Lzma2Options;
use sevenz_rust::{ArchiveEntry, ArchiveReader, ArchiveWriter, Password};
use walkdir::WalkDir;

use crate::error::UpdaterError;

/// 解压与下载共享的累计配额，防止压缩炸弹和磁盘耗尽。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractionLimits {
    pub max_total_bytes: u64,
    pub max_entries: usize,
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            max_entries: 100_000,
        }
    }
}

impl ExtractionLimits {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// 单次解压贯穿的工具归属与配额上下文。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtractionContext<'a> {
    pub(crate) tool_id: Option<&'a str>,
    pub(crate) limits: ExtractionLimits,
}

#[derive(Default)]
pub struct ArchiveService {
    limits: ExtractionLimits,
}

impl ArchiveService {
    pub fn with_limits(limits: ExtractionLimits) -> Self {
        Self { limits }
    }

    pub fn extract(
        &self,
        archive: &Path,
        destination: &Path,
        password: Option<&str>,
    ) -> Result<()> {
        self.extract_with(None, archive, destination, password)
    }

    /// 工具作用域解压：配额违规的错误信息会带上工具 ID。
    pub(crate) fn extract_for_tool(
        &self,
        tool_id: &str,
        archive: &Path,
        destination: &Path,
        password: Option<&str>,
    ) -> Result<()> {
        self.extract_with(Some(tool_id), archive, destination, password)
    }

    fn extract_with(
        &self,
        tool_id: Option<&str>,
        archive: &Path,
        destination: &Path,
        password: Option<&str>,
    ) -> Result<()> {
        let context = ExtractionContext {
            tool_id,
            limits: self.limits,
        };
        fs::create_dir_all(destination).with_context(|| {
            format!(
                "cannot create extraction directory {}",
                destination.display()
            )
        })?;
        let kind = archive_kind(archive).ok_or_else(|| UpdaterError::Archive {
            path: archive.to_path_buf(),
            message: "unsupported archive extension".to_owned(),
        })?;

        match kind {
            ArchiveKind::Zip => extract::zip(archive, destination, password, &context),
            ArchiveKind::SevenZip => extract::seven_zip(archive, destination, password, &context),
            ArchiveKind::Rar => extract::rar(archive, destination, password, &context),
            ArchiveKind::TarGz => extract::tar_gzip(archive, destination, &context),
            ArchiveKind::TarBz2 => extract::tar_bzip2(archive, destination, &context),
            ArchiveKind::TarXz => extract::tar_xz(archive, destination, &context),
            ArchiveKind::Gzip => extract::gzip(archive, destination, &context),
            ArchiveKind::Xz => extract::xz(archive, destination, &context),
        }
    }

    pub fn is_supported(&self, path: &Path) -> bool {
        archive_kind(path).is_some()
    }

    pub fn compress_7z(&self, source: &Path, destination: &Path) -> Result<()> {
        self.compress_7z_with_threads(source, destination, 1)
    }

    pub fn compress_7z_with_threads(
        &self,
        source: &Path,
        destination: &Path,
        threads: usize,
    ) -> Result<()> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create archive directory {}", parent.display()))?;
        }
        let directories = if source.is_dir() {
            WalkDir::new(source)
                .min_depth(1)
                .into_iter()
                .filter_map(|entry| match entry {
                    Ok(entry) if entry.file_type().is_dir() => Some(Ok(entry.into_path())),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let result = (|| {
            let mut writer = ArchiveWriter::create(destination)?;
            let threads = u32::try_from(threads.max(1)).unwrap_or(u32::MAX);
            // 单线程不设置分块参数，避免无并行收益时仍切换 LZMA2 流边界。
            let lzma = if threads == 1 {
                Lzma2Options::from_level(6)
            } else {
                Lzma2Options::from_level_mt(6, threads, 16 * 1024 * 1024)
            };
            writer.set_content_methods(vec![lzma.into()]);
            for directory in &directories {
                let name = directory
                    .strip_prefix(source)
                    .expect("walked entries stay under their root")
                    .to_string_lossy()
                    .into_owned();
                let directory = ArchiveEntry::from_path(directory, name);
                writer.push_archive_entry::<&[u8]>(directory, None)?;
            }
            writer.push_source_path(source, |_| true)?;
            writer.finish()?;
            Ok::<(), sevenz_rust::Error>(())
        })();
        result.map_err(|error| {
            anyhow::Error::from(UpdaterError::Archive {
                path: source.to_path_buf(),
                message: format!("7z compression failed: {error}"),
            })
        })?;
        self.verify_7z(destination)?;
        Ok(())
    }

    // 压缩完成后重新读取所有条目，触发解码和校验，避免损坏归档进入安装状态。
    pub(crate) fn verify_7z(
        &self,
        archive: &Path,
    ) -> std::result::Result<(), ArchiveVerificationError> {
        let result = (|| {
            let mut reader = ArchiveReader::open(archive, Password::empty())?;
            // 多个工具可并发校验；每个归档限制为单线程，避免按任务数重复占满 CPU。
            reader.set_thread_count(1);
            reader.for_each_entries(|_, contents| {
                // 必须消费完整解码流，读取头信息本身不会触发每个条目的 CRC 校验。
                io::copy(contents, &mut io::sink())?;
                Ok(true)
            })
        })();
        result.map_err(|error| ArchiveVerificationError {
            invalid: invalid_7z_contents(&error),
            error: UpdaterError::Archive {
                path: archive.to_path_buf(),
                message: format!("7z verification failed: {error}"),
            },
        })
    }
}

#[derive(Debug)]
pub(crate) struct ArchiveVerificationError {
    error: UpdaterError,
    invalid: bool,
}

impl ArchiveVerificationError {
    pub(crate) fn is_invalid(&self) -> bool {
        self.invalid
    }
}

impl fmt::Display for ArchiveVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ArchiveVerificationError {}

fn invalid_7z_contents(error: &sevenz_rust::Error) -> bool {
    use sevenz_rust::Error;

    // 文件访问、密码、内存限制和不支持的能力不代表归档已损坏，不能据此丢弃已有内容。
    matches!(
        error,
        Error::BadSignature(_)
            | Error::ChecksumVerificationFailed
            | Error::NextHeaderCrcMismatch
            | Error::Other(_)
            | Error::BadTerminatedStreamsInfo(_)
            | Error::BadTerminatedUnpackInfo
            | Error::BadTerminatedPackInfo(_)
            | Error::BadTerminatedSubStreamsInfo
            | Error::BadTerminatedHeader(_)
    ) || matches!(
        error,
        Error::Io(error, _)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidData
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::Other
            )
    )
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    Zip,
    SevenZip,
    Rar,
    TarGz,
    TarBz2,
    TarXz,
    Gzip,
    Xz,
}

const ARCHIVE_FORMATS: &[(&str, ArchiveKind)] = &[
    (".tar.bz2", ArchiveKind::TarBz2),
    (".tar.gz", ArchiveKind::TarGz),
    (".tar.xz", ArchiveKind::TarXz),
    (".tbz", ArchiveKind::TarBz2),
    (".tgz", ArchiveKind::TarGz),
    (".txz", ArchiveKind::TarXz),
    (".zip", ArchiveKind::Zip),
    (".rar", ArchiveKind::Rar),
    (".7z", ArchiveKind::SevenZip),
    (".gz", ArchiveKind::Gzip),
    (".xz", ArchiveKind::Xz),
];

fn archive_kind(path: &Path) -> Option<ArchiveKind> {
    archive_format(path).map(|(_, kind)| kind)
}

pub(crate) fn archive_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let suffix = archive_format(path).map(|(suffix, _)| suffix);
    Some(match suffix {
        Some(suffix) => name[..name.len() - suffix.len()].to_owned(),
        None => name.into_owned(),
    })
}

fn archive_format(path: &Path) -> Option<(&'static str, ArchiveKind)> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    ARCHIVE_FORMATS
        .iter()
        .copied()
        .find(|(suffix, _)| name.ends_with(suffix))
}
