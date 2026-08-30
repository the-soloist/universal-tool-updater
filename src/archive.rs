mod extract;
#[cfg(test)]
mod tests;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use sevenz_rust::encoder_options::Lzma2Options;
use sevenz_rust::{ArchiveEntry, ArchiveWriter};
use walkdir::WalkDir;

use crate::domain::ExtractionLimits;
use crate::error::UpdaterError;

/// Cumulative extraction quotas enforced across every archive entry before
/// decompression, and mirrored onto download content lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_total_bytes: u64,
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            max_entries: 100_000,
        }
    }
}

impl From<ExtractionLimits> for Limits {
    fn from(limits: ExtractionLimits) -> Self {
        Self {
            max_total_bytes: limits.max_total_bytes,
            max_entries: limits.max_entries,
        }
    }
}

/// Tool attribution and symlink policy threaded through a single extraction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtractionContext<'a> {
    pub(crate) tool_id: Option<&'a str>,
    pub(crate) allow_symlinks: bool,
    pub(crate) limits: Limits,
}

#[derive(Default)]
pub struct ArchiveService {
    limits: Limits,
}

impl ArchiveService {
    pub fn with_limits(limits: Limits) -> Self {
        Self { limits }
    }

    pub fn extract(
        &self,
        archive: &Path,
        destination: &Path,
        password: Option<&str>,
    ) -> Result<()> {
        self.extract_with(
            ExtractionContext {
                tool_id: None,
                allow_symlinks: false,
                limits: self.limits,
            },
            archive,
            destination,
            password,
        )
    }

    /// Tool-scoped extraction: quota violations name the tool and archives
    /// containing links are only permitted when the tool opted in.
    pub(crate) fn extract_for_tool(
        &self,
        tool_id: &str,
        allow_symlinks: bool,
        archive: &Path,
        destination: &Path,
        password: Option<&str>,
    ) -> Result<()> {
        self.extract_with(
            ExtractionContext {
                tool_id: Some(tool_id),
                allow_symlinks,
                limits: self.limits,
            },
            archive,
            destination,
            password,
        )
    }

    fn extract_with(
        &self,
        context: ExtractionContext<'_>,
        archive: &Path,
        destination: &Path,
        password: Option<&str>,
    ) -> Result<()> {
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
            writer.set_content_methods(vec![
                Lzma2Options::from_level_mt(6, threads, 16 * 1024 * 1024).into(),
            ]);
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
            UpdaterError::Archive {
                path: source.to_path_buf(),
                message: format!("7z compression failed: {error}"),
            }
            .into()
        })
    }
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
