use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Component, Path};

use anyhow::Result;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::Archive as TarArchive;
use unrar_rs::{ExtractOptions, RarArchive};
use xz2::read::XzDecoder;

use crate::error::UpdaterError;

pub(super) fn zip(archive_path: &Path, destination: &Path, password: Option<&str>) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive =
        zip::ZipArchive::new(BufReader::new(file)).map_err(|error| UpdaterError::Archive {
            path: archive_path.to_path_buf(),
            message: error.to_string(),
        })?;
    for index in 0..archive.len() {
        let entry = if let Some(password) = password {
            archive.by_index_decrypt(index, password.as_bytes())
        } else {
            archive.by_index(index)
        };
        let mut entry = entry.map_err(|error| UpdaterError::Archive {
            path: archive_path.to_path_buf(),
            message: error.to_string(),
        })?;
        let enclosed = entry.enclosed_name().ok_or_else(|| UpdaterError::Archive {
            path: archive_path.to_path_buf(),
            message: format!("unsafe ZIP entry {:?}", entry.name()),
        })?;
        let output = destination.join(enclosed);
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
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output, fs::Permissions::from_mode(mode))?;
        }
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
