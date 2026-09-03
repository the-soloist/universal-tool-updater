use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tempfile::tempdir;
use zip::write::SimpleFileOptions;

use super::{ArchiveService, ExtractionLimits, archive_stem};

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

    ArchiveService::default()
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

    ArchiveService::default()
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
        ArchiveService::default()
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

    ArchiveService::default()
        .compress_7z_with_threads(&source, &archive, 2)
        .unwrap();
    ArchiveService::default()
        .extract(&archive, &output, None)
        .unwrap();
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
    ArchiveService::default()
        .compress_7z(&source, &archive)
        .unwrap();

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

    let error = ArchiveService::default().verify_7z(&archive).unwrap_err();
    assert!(error.is_invalid(), "{error:?}");
}

#[test]
fn does_not_classify_a_missing_7z_archive_as_corrupt() {
    let directory = tempdir().unwrap();
    let archive = directory.path().join("missing.7z");

    let error = ArchiveService::default().verify_7z(&archive).unwrap_err();
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

    ArchiveService::default()
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
        ArchiveService::default()
            .extract(&archive, &output, None)
            .unwrap();
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
        ArchiveService::default()
            .extract(&archive, &output, None)
            .unwrap();
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
        assert!(
            ArchiveService::default().is_supported(Path::new(name)),
            "{name}"
        );
    }
    assert!(!ArchiveService::default().is_supported(Path::new("tool.exe")));
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

    ArchiveService::default()
        .extract(&archive_path, &output_path, None)
        .unwrap();

    let mode = fs::metadata(output_path.join("tool"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
}

#[test]
fn rejects_archives_exceeding_the_byte_quota() {
    let directory = tempdir().unwrap();
    let output_path = directory.path().join("output");
    let service = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 4,
        max_entries: 100,
    });

    let archive_path = directory.path().join("tool.zip");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("tool.txt", SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut archive, b"0123456789").unwrap();
    archive.finish().unwrap();
    let error = service
        .extract_for_tool("demo", &archive_path, &output_path, None)
        .unwrap_err();
    assert!(
        error.to_string().contains("tool demo"),
        "expected tool attribution, got {error:#}"
    );
    assert!(
        error.to_string().contains("tool.txt"),
        "expected the entry name, got {error:#}"
    );
    assert!(
        error.to_string().contains("max_total_bytes 4"),
        "expected the quota limit, got {error:#}"
    );

    let single = directory.path().join("tool.bin.gz");
    fs::write(&single, gzip(b"0123456789")).unwrap();
    assert!(
        service
            .extract_for_tool("demo", &single, &output_path, None)
            .is_err()
    );
}

#[test]
fn rejects_archives_exceeding_the_entry_quota() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("tool.zip");
    let output_path = directory.path().join("output");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    for name in ["a.txt", "b.txt"] {
        archive
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut archive, b"ok").unwrap();
    }
    archive.finish().unwrap();

    let service = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 1024,
        max_entries: 1,
    });
    let error = service
        .extract_for_tool("demo", &archive_path, &output_path, None)
        .unwrap_err();
    assert!(
        error.to_string().contains("max_entries 1"),
        "expected the entry quota, got {error:#}"
    );
}

#[test]
fn interrupts_zip_entries_whose_real_output_exceeds_the_quota() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("lying.zip");
    let output_path = directory.path().join("output");

    // Deflate 流实际解压 64 KiB，而两个 ZIP 头都声明 4 字节，
    // 只有输出侧账本能在写入中途发现这种谎言。
    let payload = vec![0_u8; 64 * 1024];
    let mut deflater =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    deflater.write_all(&payload).unwrap();
    let compressed = deflater.finish().unwrap();
    let crc = crc_fast::crc32_iso_hdlc(&payload) as u32;
    fs::write(
        &archive_path,
        build_lying_deflate_zip("tool.txt", &compressed, crc, 4),
    )
    .unwrap();

    let service = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 16 * 1024,
        max_entries: 100,
    });
    let error = service
        .extract_for_tool("demo", &archive_path, &output_path, None)
        .unwrap_err();
    assert!(
        error.to_string().contains("max_total_bytes 16384"),
        "expected the output quota, got {error:#}"
    );
    let written = fs::metadata(output_path.join("tool.txt"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    assert!(
        written <= 16 * 1024,
        "the interrupted entry wrote {written} bytes past the quota"
    );
}

#[test]
fn rejects_pax_size_overrides_before_writing_tar_entries() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("pax-size.tar.gz");
    let output_path = directory.path().join("output");
    let contents = b"0123456789";
    let tar = pax_size_tar();

    // The raw file header declares zero bytes while the PAX extension supplies
    // the effective size. The quota must use the effective value before unpack.
    {
        let mut parsed = tar::Archive::new(tar.as_slice());
        let entry = parsed.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.header().size().unwrap(), 0);
        assert_eq!(entry.size(), contents.len() as u64);
    }
    fs::write(&archive_path, gzip(&tar)).unwrap();

    let service = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 4,
        max_entries: 100,
    });
    let error = service
        .extract_for_tool("demo", &archive_path, &output_path, None)
        .unwrap_err();

    assert!(
        error.to_string().contains("max_total_bytes 4"),
        "expected the effective PAX size to exceed the quota, got {error:#}"
    );
    assert!(
        !output_path.join("tool.txt").exists(),
        "the oversized PAX entry must be rejected before creating its output file"
    );
}

#[test]
fn interrupts_rar_members_with_unknown_size_at_the_quota() {
    let directory = tempdir().unwrap();
    let archive_path = directory.path().join("unknown.rar");
    let output_path = directory.path().join("output");
    // 未声明 unpacked size 的成员在声明通道计 0 字节，配额前置检查
    // 依赖库级 max_unpacked_size 在写盘中途拦截，而非等完整落盘后复核。
    let payload = vec![0_u8; 64 * 1024];
    fs::write(
        &archive_path,
        stored_rar5_unknown_size("tool.txt", &payload),
    )
    .unwrap();

    let service = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 16 * 1024,
        max_entries: 100,
    });
    let error = service
        .extract_for_tool("demo", &archive_path, &output_path, None)
        .unwrap_err();
    assert!(
        error.to_string().contains("tool demo"),
        "expected tool attribution, got {error:#}"
    );
    assert!(
        error.to_string().contains("16384"),
        "expected the library-level output limit, got {error:#}"
    );
    let written = fs::metadata(output_path.join("tool.txt"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    assert!(
        written <= 16 * 1024,
        "the interrupted member wrote {written} bytes past the quota"
    );
}

#[test]
fn extracts_a_7z_archive_whose_output_equals_the_quota() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let archive = directory.path().join("source.7z");
    let output = directory.path().join("output");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("tool.txt"), "0123456789").unwrap();
    ArchiveService::default()
        .compress_7z(&source, &archive)
        .unwrap();

    // 累计输出恰好等于配额是合法归档：读取侧守卫在配额耗尽后
    // 仍要确认底层 EOF，不能把等额输出误判为超额。
    let exact = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 10,
        max_entries: 100,
    });
    exact.extract(&archive, &output, None).unwrap();
    assert_eq!(
        fs::read_to_string(output.join("tool.txt")).unwrap(),
        "0123456789"
    );

    let below = ArchiveService::with_limits(ExtractionLimits {
        max_total_bytes: 9,
        max_entries: 100,
    });
    let output_below = directory.path().join("output-below");
    assert!(
        below.extract(&archive, &output_below, None).is_err(),
        "an archive one byte over the quota must fail"
    );
}

/// 构造本地头与中央头都声明（谎报的）解压大小、
/// 但携带更大 Deflate 载荷的 stored ZIP。
fn build_lying_deflate_zip(name: &str, compressed: &[u8], crc: u32, declared_size: u32) -> Vec<u8> {
    let name = name.as_bytes();
    let mut zip = Vec::new();
    let local = [
        &0x04034b50_u32.to_le_bytes()[..],
        &20_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &8_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &crc.to_le_bytes(),
        &(compressed.len() as u32).to_le_bytes(),
        &declared_size.to_le_bytes(),
        &(name.len() as u16).to_le_bytes(),
        &0_u16.to_le_bytes(),
    ]
    .concat();
    zip.extend_from_slice(&local);
    zip.extend_from_slice(name);
    zip.extend_from_slice(compressed);
    let data_offset = (local.len() + name.len() + compressed.len()) as u32;
    let central = [
        &0x02014b50_u32.to_le_bytes()[..],
        &0x0314_u16.to_le_bytes(),
        &20_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &8_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &0x21_u16.to_le_bytes(),
        &crc.to_le_bytes(),
        &(compressed.len() as u32).to_le_bytes(),
        &declared_size.to_le_bytes(),
        &(name.len() as u16).to_le_bytes(),
        &0_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &0x81a4_u32.to_le_bytes(),
        &0_u32.to_le_bytes(),
    ]
    .concat();
    zip.extend_from_slice(&central);
    zip.extend_from_slice(name);
    let end = [
        &0x06054b50_u32.to_le_bytes()[..],
        &0_u16.to_le_bytes(),
        &0_u16.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &((46 + name.len()) as u32).to_le_bytes(),
        &data_offset.to_le_bytes(),
        &0_u16.to_le_bytes(),
    ]
    .concat();
    zip.extend_from_slice(&end);
    zip
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

fn pax_size_tar() -> Vec<u8> {
    const TAR_BLOCK_BYTES: usize = 512;

    let filename = "tool.txt";
    let contents = b"0123456789";
    let pax_record = b"11 size=10\n";
    let mut output = Vec::new();

    let mut pax_header = tar::Header::new_ustar();
    pax_header.set_path("PaxHeaders/tool.txt").unwrap();
    pax_header.set_size(pax_record.len() as u64);
    pax_header.set_mode(0o644);
    pax_header.set_entry_type(tar::EntryType::XHeader);
    pax_header.set_cksum();
    output.extend_from_slice(pax_header.as_bytes());
    output.extend_from_slice(pax_record);
    output.resize(output.len().next_multiple_of(TAR_BLOCK_BYTES), 0);

    let mut file_header = tar::Header::new_ustar();
    file_header.set_path(filename).unwrap();
    file_header.set_size(0);
    file_header.set_mode(0o644);
    file_header.set_entry_type(tar::EntryType::Regular);
    file_header.set_cksum();
    output.extend_from_slice(file_header.as_bytes());
    output.extend_from_slice(contents);
    output.resize(output.len().next_multiple_of(TAR_BLOCK_BYTES), 0);
    output.resize(output.len() + TAR_BLOCK_BYTES * 2, 0);
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
    stored_rar5_with_flags(filename, content, 0x0004)
}

/// 构造 unpacked size 未声明的 stored RAR5 成员：file flags 置
/// UNPACKED_SIZE_UNKNOWN（0x0008）后解析器忽略 size 字段，
/// 声明通道对其计 0 字节，只有输出侧限制能在写盘中途拦截。
fn stored_rar5_unknown_size(filename: &str, content: &[u8]) -> Vec<u8> {
    stored_rar5_with_flags(filename, content, 0x0004 | 0x0008)
}

fn stored_rar5_with_flags(filename: &str, content: &[u8], file_flags: u64) -> Vec<u8> {
    let mut archive = b"Rar!\x1a\x07\x01\x00".to_vec();
    archive.extend_from_slice(&rar5_header(1, 0, &encode_vint(0)));

    let mut file_body = Vec::new();
    file_body.extend_from_slice(&encode_vint(file_flags));
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
