mod extract;
#[cfg(test)]
mod tests;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use sevenz_rust::encoder_options::Lzma2Options;
use sevenz_rust::{ArchiveEntry, ArchiveWriter};
use walkdir::WalkDir;

use crate::error::UpdaterError;

pub struct ArchiveService;

impl ArchiveService {
    pub fn extract(
        &self,
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
            ArchiveKind::Zip => extract::zip(archive, destination, password),
            ArchiveKind::SevenZip => extract::seven_zip(archive, destination, password),
            ArchiveKind::Rar => extract::rar(archive, destination, password),
            ArchiveKind::TarGz => extract::tar_gzip(archive, destination),
            ArchiveKind::TarBz2 => extract::tar_bzip2(archive, destination),
            ArchiveKind::TarXz => extract::tar_xz(archive, destination),
            ArchiveKind::Gzip => extract::gzip(archive, destination),
            ArchiveKind::Xz => extract::xz(archive, destination),
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
