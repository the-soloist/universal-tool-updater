use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
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
fn extracts_zip_entries_compressed_with_lzma() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("lzma.zip");
    let output_path = directory.path().join("output");
    fs::write(
        &archive_path,
        [
            0x50, 0x4b, 0x03, 0x04, 0x3f, 0x00, 0x02, 0x00, 0x0e, 0x00, 0x6a, 0xb7, 0x1c, 0x5d,
            0x8b, 0x20, 0x37, 0x03, 0x20, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x08, 0x00,
            0x00, 0x00, 0x74, 0x6f, 0x6f, 0x6c, 0x2e, 0x74, 0x78, 0x74, 0x09, 0x04, 0x05, 0x00,
            0x5d, 0x00, 0x00, 0x80, 0x00, 0x00, 0x36, 0x1e, 0x89, 0xdd, 0x7d, 0x49, 0x62, 0x6c,
            0x0c, 0xb6, 0x91, 0x06, 0x29, 0x06, 0x8b, 0x59, 0xff, 0xff, 0x7c, 0x94, 0x00, 0x00,
            0x50, 0x4b, 0x01, 0x02, 0x3f, 0x03, 0x3f, 0x00, 0x02, 0x00, 0x0e, 0x00, 0x6a, 0xb7,
            0x1c, 0x5d, 0x8b, 0x20, 0x37, 0x03, 0x20, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00,
            0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x01,
            0x00, 0x00, 0x00, 0x00, 0x74, 0x6f, 0x6f, 0x6c, 0x2e, 0x74, 0x78, 0x74, 0x50, 0x4b,
            0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x36, 0x00, 0x00, 0x00,
            0x46, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    )
    .unwrap();

    ArchiveService
        .extract(&archive_path, &output_path, None)
        .unwrap();
    assert_eq!(
        fs::read_to_string(output_path.join("tool.txt")).unwrap(),
        "lzma payload"
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
fn rejects_a_7z_archive_with_corrupted_contents() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let archive = directory.path().join("source.7z");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("tool.txt"), "payload").unwrap();
    ArchiveService.compress_7z(&source, &archive).unwrap();

    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&archive)
        .unwrap();
    file.seek(SeekFrom::Start(32)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(32)).unwrap();
    file.write_all(&[byte[0] ^ 0xff]).unwrap();
    file.sync_all().unwrap();

    let error = ArchiveService.verify_7z(&archive).unwrap_err();
    assert!(error.is_invalid(), "{error:?}");
}

#[test]
fn does_not_classify_a_missing_7z_archive_as_corrupt() {
    let directory = tempdir().unwrap();
    let archive = directory.path().join("missing.7z");

    let error = ArchiveService.verify_7z(&archive).unwrap_err();
    assert!(!error.is_invalid());
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

#[test]
fn extracts_all_tar_and_single_stream_formats() {
    let directory = tempdir().unwrap();
    let tar = tar_fixture();
    let archives = [
        ("tool.tar.gz", gzip(&tar)),
        ("tool.tar.bz2", bzip2(&tar)),
        ("tool.tar.xz", xz(&tar)),
    ];
    for (name, contents) in archives {
        let archive = directory.path().join(name);
        let output = directory.path().join(format!("output-{name}"));
        fs::write(&archive, contents).unwrap();
        ArchiveService.extract(&archive, &output, None).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("bin/tool.txt")).unwrap(),
            "payload",
            "failed to extract {name}"
        );
    }

    for (name, contents) in [
        ("tool.bin.gz", gzip(b"payload")),
        ("tool.bin.xz", xz(b"payload")),
    ] {
        let archive = directory.path().join(name);
        let output = directory.path().join(format!("output-{name}"));
        fs::write(&archive, contents).unwrap();
        ArchiveService.extract(&archive, &output, None).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("tool.bin")).unwrap(),
            "payload",
            "failed to extract {name}"
        );
    }
}

#[test]
fn recognizes_every_documented_archive_extension_case_insensitively() {
    for name in [
        "tool.ZIP",
        "tool.RAR",
        "tool.7Z",
        "tool.tar.gz",
        "tool.tar.bz2",
        "tool.tar.xz",
        "tool.tgz",
        "tool.tbz",
        "tool.txz",
        "tool.gz",
        "tool.xz",
    ] {
        assert!(ArchiveService.is_supported(Path::new(name)), "{name}");
    }
    assert!(!ArchiveService.is_supported(Path::new("tool.exe")));
}

#[cfg(unix)]
#[test]
fn strips_privilege_bits_from_zip_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("privileged.zip");
    let output_path = directory.path().join("output");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file(
            "tool",
            SimpleFileOptions::default().unix_permissions(0o4755),
        )
        .unwrap();
    archive.write_all(b"tool").unwrap();
    archive.finish().unwrap();

    ArchiveService
        .extract(&archive_path, &output_path, None)
        .unwrap();

    let mode = fs::metadata(output_path.join("tool"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
}

fn tar_fixture() -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut archive = tar::Builder::new(&mut output);
        let contents = b"payload";
        let mut header = tar::Header::new_gnu();
        header.set_path("bin/tool.txt").unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append(&header, contents.as_slice()).unwrap();
        archive.finish().unwrap();
    }
    output
}

fn gzip(contents: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(contents).unwrap();
    encoder.finish().unwrap()
}

fn bzip2(contents: &[u8]) -> Vec<u8> {
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(contents).unwrap();
    encoder.finish().unwrap()
}

fn xz(contents: &[u8]) -> Vec<u8> {
    let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
    encoder.write_all(contents).unwrap();
    encoder.finish().unwrap()
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
