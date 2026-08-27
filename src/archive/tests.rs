use std::fs;
use std::path::Path;

use tempfile::tempdir;
use zip::write::SimpleFileOptions;

use super::{ArchiveService, archive_stem};

#[test]
fn recognizes_compound_archive_stems() {
    assert_eq!(
        archive_stem(Path::new("tool.tar.gz")).as_deref(),
        Some("tool")
    );
    assert_eq!(archive_stem(Path::new("tool.tgz")).as_deref(), Some("tool"));
    assert_eq!(
        archive_stem(Path::new("tool.bin")).as_deref(),
        Some("tool.bin")
    );
}

#[test]
fn extracts_zip_without_leaving_destination() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("sample.zip");
    let output_path = directory.path().join("output");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("folder/tool.txt", SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut archive, b"ok").unwrap();
    archive.finish().unwrap();

    ArchiveService
        .extract(&archive_path, &output_path, None)
        .unwrap();
    assert_eq!(
        fs::read_to_string(output_path.join("folder/tool.txt")).unwrap(),
        "ok"
    );
}

#[test]
fn rejects_zip_entries_that_escape_the_destination() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("unsafe.zip");
    let output_path = directory.path().join("output");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("../outside.txt", SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut archive, b"unsafe").unwrap();
    archive.finish().unwrap();

    assert!(
        ArchiveService
            .extract(&archive_path, &output_path, None)
            .is_err()
    );
    assert!(!directory.path().join("outside.txt").exists());
}

#[test]
fn round_trips_multithreaded_7z_archives() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let archive = directory.path().join("source.7z");
    let output = directory.path().join("output");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("empty")).unwrap();
    fs::write(source.join("tool.txt"), "ok").unwrap();

    ArchiveService
        .compress_7z_with_threads(&source, &archive, 2)
        .unwrap();
    ArchiveService.extract(&archive, &output, None).unwrap();
    assert_eq!(fs::read_to_string(output.join("tool.txt")).unwrap(), "ok");
    assert!(output.join("empty").is_dir());
}

#[test]
fn extracts_rar5_with_the_rust_backend() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("sample.rar");
    let output_path = directory.path().join("output");
    fs::write(
        &archive_path,
        stored_rar5("folder/tool.txt", b"rust-native-rar"),
    )
    .unwrap();

    ArchiveService
        .extract(&archive_path, &output_path, None)
        .unwrap();
    assert_eq!(
        fs::read_to_string(output_path.join("folder/tool.txt")).unwrap(),
        "rust-native-rar"
    );
}

fn stored_rar5(filename: &str, content: &[u8]) -> Vec<u8> {
    let mut archive = b"Rar!\x1a\x07\x01\x00".to_vec();
    archive.extend_from_slice(&rar5_header(1, 0, &encode_vint(0)));

    let mut file_body = Vec::new();
    file_body.extend_from_slice(&encode_vint(0x0004));
    file_body.extend_from_slice(&encode_vint(content.len() as u64));
    file_body.extend_from_slice(&encode_vint(0o644));
    file_body.extend_from_slice(&(crc_fast::crc32_iso_hdlc(content) as u32).to_le_bytes());
    file_body.extend_from_slice(&encode_vint(0));
    file_body.extend_from_slice(&encode_vint(1));
    file_body.extend_from_slice(&encode_vint(filename.len() as u64));
    file_body.extend_from_slice(filename.as_bytes());
    archive.extend_from_slice(&rar5_header(2, content.len() as u64, &file_body));
    archive.extend_from_slice(content);
    archive.extend_from_slice(&rar5_header(5, 0, &encode_vint(0)));
    archive
}

fn rar5_header(kind: u64, data_size: u64, type_body: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_vint(kind));
    body.extend_from_slice(&encode_vint(u64::from(data_size > 0) * 0x0002));
    if data_size > 0 {
        body.extend_from_slice(&encode_vint(data_size));
    }
    body.extend_from_slice(type_body);

    let size = encode_vint(body.len() as u64);
    let mut checksummed = size.clone();
    checksummed.extend_from_slice(&body);
    let mut header = (crc_fast::crc32_iso_hdlc(&checksummed) as u32)
        .to_le_bytes()
        .to_vec();
    header.extend_from_slice(&checksummed);
    header
}

fn encode_vint(mut value: u64) -> Vec<u8> {
    let mut encoded = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        encoded.push(byte);
        if value == 0 {
            return encoded;
        }
    }
}
