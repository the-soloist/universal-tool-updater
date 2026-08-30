//! Deep-analysis benchmark suite backing docs/algorithms-deep-analysis.md
//! (dimensions D1, D2, D3, D4, D6).
//!
//! Follow-up to tests/algorithm_benchmarks.rs (first-round adjudication):
//! - D1 sweeps the zip extraction read/write buffer size across eight tiers
//!   (8 KiB production anchor plus 16..1024 KiB) to locate the benefit knee.
//! - D2 stacks the adopted A1 (download/hash pipelining) and A2 (extraction
//!   buffers) along a synthetic single-tool pipeline and checks whether the
//!   combined gain stays additive.
//! - D3 re-validates the A2 buffers with four concurrent extraction workers.
//! - D4 prices the hard_link-first copy_tree variant: same-volume links,
//!   cross-volume fallback copies, and the failed-syscall latency.
//! - D6 measures how the A15 LZMA decoder swap scales with the LZMA share of
//!   a mixed-method zip (zip crate 2.4 cannot write LZMA members, so the
//!   mixed archives are emitted by a minimal raw ZIP writer living here).
//!
//! Every benchmark replicates the current production code path (no production
//! code is modified; optimized variants exist here as controlled counterparts
//! only). Equivalence (digest / tree manifest / CRC) is asserted before any
//! timing is collected; timings are medians over ITERATIONS runs taken with
//! std::time::Instant.
//!
//! All tests carry #[ignore] so the regular `cargo test` gate stays fast. Run
//! sequentially for stable numbers (the tiers share one temp volume):
//!     cargo test --release --test algorithm_deep_analysis -- \
//!         --ignored --nocapture --test-threads=1

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use crc_fast::{CrcAlgorithm, Digest as CrcDigest};
use flate2::Compression;
use flate2::write::DeflateEncoder;
use lzma_rs::decompress::raw::{LzmaDecoder, LzmaParams, LzmaProperties};
use sha2::{Digest, Sha256};
use tempfile::{Builder, tempdir};
use walkdir::WalkDir;
use xz2::read::XzDecoder;
use xz2::stream::{LzmaOptions, Stream as XzStream};
use xz2::write::XzEncoder;
use zip::read::ZipArchive;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const MIB: u64 = 1024 * 1024;
const ITERATIONS: usize = 3;
const PATTERN_BLOCK: u64 = 4096;
/// A2 candidate carried through D2/D3; D1 decides whether this is the knee.
const A2_TIER: usize = 256 * 1024;

fn emit(tag: &str, message: &str) {
    println!("[{tag}] {message}");
}

fn median_ms(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

fn median_u64(mut values: Vec<u64>) -> u64 {
    values.sort();
    values[values.len() / 2]
}

fn median_f(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite sample"));
    samples[samples.len() / 2]
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Deterministic splitmix64 PRNG for benchmark payload generation.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(8) {
            let value = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
    }
}

/// Cycled 1 MiB pseudo-random base pattern with light per-4-KiB mutation:
/// adjacent blocks stay mostly identical so deflate sees a realistic (near
/// stored) structure while LZMA sees long matches, yet every block hashes
/// differently. Same generator family as the first-round suite.
struct PatternSource {
    base: Vec<u8>,
}

impl PatternSource {
    fn new(seed: u64) -> Self {
        let mut base = vec![0_u8; MIB as usize];
        SplitMix64(seed).fill(&mut base);
        Self { base }
    }

    fn fill(&self, chunk: &mut [u8], offset: u64) {
        let base_len = self.base.len() as u64;
        let mut copied = 0_usize;
        let mut start = (offset % base_len) as usize;
        while copied < chunk.len() {
            let count = (chunk.len() - copied).min(self.base.len() - start);
            chunk[copied..copied + count].copy_from_slice(&self.base[start..start + count]);
            copied += count;
            start = 0;
        }
        let first_block = offset / PATTERN_BLOCK;
        let last_block = (offset + chunk.len() as u64 - 1) / PATTERN_BLOCK;
        for block in first_block..=last_block {
            let absolute = block * PATTERN_BLOCK;
            if absolute < offset {
                continue;
            }
            let local = (absolute - offset) as usize;
            if local + 8 <= chunk.len() {
                chunk[local..local + 8].copy_from_slice(&block.to_le_bytes());
            }
        }
    }
}

/// Reader serving `length` bytes of pattern data starting at `start`, 1 MiB
/// per refill; stands in for a response body or a member generator.
struct PatternReader<'a> {
    source: &'a PatternSource,
    offset: u64,
    remaining: u64,
    chunk: Vec<u8>,
    filled: usize,
    consumed: usize,
}

impl<'a> PatternReader<'a> {
    fn new(source: &'a PatternSource, start: u64, length: u64) -> Self {
        Self {
            source,
            offset: start,
            remaining: length,
            chunk: vec![0_u8; MIB as usize],
            filled: 0,
            consumed: 0,
        }
    }
}

impl Read for PatternReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.consumed == self.filled {
            if self.remaining == 0 {
                return Ok(0);
            }
            let take = self.remaining.min(self.chunk.len() as u64) as usize;
            self.source.fill(&mut self.chunk[..take], self.offset);
            self.filled = take;
            self.consumed = 0;
            self.offset += take as u64;
            self.remaining -= take as u64;
        }
        let available = self.filled - self.consumed;
        let count = buffer.len().min(available);
        buffer[..count].copy_from_slice(&self.chunk[self.consumed..self.consumed + count]);
        self.consumed += count;
        Ok(count)
    }
}

/// Replica of downloader.rs verify_sha256: reopen the finished file and hash
/// it end to end through a 64 KiB BufReader loop.
fn verify_sha256_replica(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

/// Sorted (relative path, size, content SHA-256) list describing a file tree.
type TreeManifest = Vec<(String, u64, String)>;

fn tree_manifest(root: &Path) -> io::Result<TreeManifest> {
    let mut manifest = Vec::new();
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .to_string_lossy()
            .replace('\\', "/");
        manifest.push((
            relative,
            entry.metadata()?.len(),
            verify_sha256_replica(entry.path())?,
        ));
    }
    manifest.sort();
    Ok(manifest)
}

/// Free bytes on the volume holding `path`.
#[cfg(windows)]
fn volume_free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_available: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut free_available = 0_u64;
    let mut total = 0_u64;
    let mut total_free = 0_u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            &mut total,
            &mut total_free,
        )
    };
    (ok != 0).then_some(total_free)
}

#[cfg(not(windows))]
fn volume_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Resolves the large tier: 1 GiB when the temp volume has headroom for the
/// archive plus extracted copies, otherwise 512 MiB.
fn resolve_large_mib(temp_root: &Path) -> u64 {
    let required: u64 = 6 * 1024 * MIB;
    match volume_free_bytes(temp_root) {
        Some(free) if free < required => {
            emit(
                "env",
                &format!("free_bytes={free} below {required}; large tier downgraded to 512 MiB"),
            );
            512
        }
        _ => 1024,
    }
}

// ---------------------------------------------------------------------------
// Shared extraction machinery (production replica of archive/extract.rs zip())
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct IoCounters {
    write_calls: Rc<Cell<u64>>,
    write_bytes: Rc<Cell<u64>>,
    read_calls: Rc<Cell<u64>>,
}

/// File wrapper counting actual read syscalls issued to the OS.
struct CountingFileReader {
    inner: File,
    counters: IoCounters,
}

impl CountingFileReader {
    fn open(path: &Path, counters: IoCounters) -> io::Result<Self> {
        Ok(Self {
            inner: File::open(path)?,
            counters,
        })
    }
}

impl Read for CountingFileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.counters
            .read_calls
            .set(self.counters.read_calls.get() + 1);
        Ok(read)
    }
}

impl Seek for CountingFileReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

/// File wrapper counting actual write syscalls issued to the OS.
struct CountingFileWriter {
    inner: File,
    counters: IoCounters,
}

impl CountingFileWriter {
    fn create(path: &Path, counters: IoCounters) -> io::Result<Self> {
        Ok(Self {
            inner: File::create(path)?,
            counters,
        })
    }
}

impl Write for CountingFileWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.counters
            .write_calls
            .set(self.counters.write_calls.get() + 1);
        self.counters
            .write_bytes
            .set(self.counters.write_bytes.get() + written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Minimal replica of the production CountingWriter (quota accounting
/// stripped, passthrough shape kept) so the write path matches extract.rs.
struct CountingSink<W> {
    inner: W,
    written: u64,
}

impl<W: Write> CountingSink<W> {
    fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }

    fn written(&self) -> u64 {
        self.written
    }
}

impl<W: Write> Write for CountingSink<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct ExtractOutcome {
    elapsed: Duration,
    logical_bytes: u64,
    write_calls: u64,
    read_calls: u64,
}

/// Production-shaped zip extraction: `None` keeps the exact current buffers
/// (default 8 KiB BufReader, bare File sink, std io::copy); `Some(capacity)`
/// uses that capacity for both the archive BufReader and a BufWriter in front
/// of the counting file sink.
fn extract_zip_tier(
    archive_path: &Path,
    destination: &Path,
    tier: Option<usize>,
) -> io::Result<ExtractOutcome> {
    let counters = IoCounters::default();
    let reader = CountingFileReader::open(archive_path, counters.clone())?;
    let mut archive = match tier {
        Some(capacity) => {
            ZipArchive::new(BufReader::with_capacity(capacity, reader)).map_err(io::Error::other)?
        }
        None => ZipArchive::new(BufReader::new(reader)).map_err(io::Error::other)?,
    };
    fs::create_dir_all(destination)?;
    let mut logical = 0_u64;
    let started = Instant::now();
    for index in 0..archive.len() {
        // Production fetches header info via by_index_raw first (quota charge).
        let size = {
            let entry = archive.by_index_raw(index).map_err(io::Error::other)?;
            entry.size()
        };
        let mut entry = archive.by_index(index).map_err(io::Error::other)?;
        if entry.is_dir() {
            continue;
        }
        let output = destination.join(entry.name());
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let written = match tier {
            Some(capacity) => {
                let sink = CountingFileWriter::create(&output, counters.clone())?;
                let mut limited = CountingSink::new(BufWriter::with_capacity(capacity, sink));
                io::copy(&mut entry, &mut limited)?;
                limited.flush()?;
                limited.written()
            }
            None => {
                let sink = CountingFileWriter::create(&output, counters.clone())?;
                let mut limited = CountingSink::new(sink);
                io::copy(&mut entry, &mut limited)?;
                limited.written()
            }
        };
        assert_eq!(written, size);
        logical += written;
    }
    Ok(ExtractOutcome {
        elapsed: started.elapsed(),
        logical_bytes: logical,
        write_calls: counters.write_calls.get(),
        read_calls: counters.read_calls.get(),
    })
}

/// Zip with one deflate member (80% of logical bytes) plus one stored member
/// (20%), the first-round B2 shape.
fn build_mixed_zip(source: &PatternSource, path: &Path, logical: u64) -> io::Result<()> {
    let deflate_len = logical * 4 / 5;
    let stored_len = logical - deflate_len;
    let file = File::create(path)?;
    let mut archive = ZipWriter::new(BufWriter::new(file));
    archive
        .start_file(
            "payload/deflated.bin",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(io::Error::other)?;
    let mut reader = PatternReader::new(source, 0, deflate_len);
    io::copy(&mut reader, &mut archive)?;
    archive
        .start_file(
            "payload/stored.bin",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .map_err(io::Error::other)?;
    let mut reader = PatternReader::new(source, deflate_len, stored_len);
    io::copy(&mut reader, &mut archive)?;
    archive.finish().map_err(io::Error::other)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// D1: buffer size sweep for the A2 extraction buffers
// ---------------------------------------------------------------------------

#[test]
#[ignore = "benchmark: run with --ignored --nocapture --test-threads=1"]
fn d1_buffer_size_sweep() {
    let tiers: [Option<usize>; 8] = [
        None,
        Some(16 * 1024),
        Some(32 * 1024),
        Some(64 * 1024),
        Some(128 * 1024),
        Some(256 * 1024),
        Some(512 * 1024),
        Some(1024 * 1024),
    ];
    let temp = tempdir().unwrap();
    let large_mib = resolve_large_mib(temp.path());
    let source = PatternSource::new(0xD1CE_0000_0000_0001);
    for &size_mib in &[256_u64, large_mib] {
        let logical = size_mib * MIB;
        let archive_path = temp.path().join(format!("d1-{size_mib}mib.zip"));
        build_mixed_zip(&source, &archive_path, logical).unwrap();
        let archive_bytes = fs::metadata(&archive_path).unwrap().len();

        // Equivalence: the production anchor and the largest tier must
        // produce identical trees before any timing is collected.
        let base_dir = temp.path().join(format!("d1-eq-base-{size_mib}mib"));
        let top_dir = temp.path().join(format!("d1-eq-top-{size_mib}mib"));
        extract_zip_tier(&archive_path, &base_dir, None).unwrap();
        extract_zip_tier(&archive_path, &top_dir, Some(1024 * 1024)).unwrap();
        assert_eq!(
            tree_manifest(&base_dir).unwrap(),
            tree_manifest(&top_dir).unwrap(),
            "D1 trees differ at {size_mib} MiB"
        );
        fs::remove_dir_all(&base_dir).unwrap();
        fs::remove_dir_all(&top_dir).unwrap();

        for tier in tiers {
            let label = tier
                .map(|capacity| format!("{}KiB", capacity / 1024))
                .unwrap_or_else(|| "prod-8KiB".to_owned());
            let mut elapsed = Vec::with_capacity(ITERATIONS);
            let mut writes = Vec::with_capacity(ITERATIONS);
            let mut reads = Vec::with_capacity(ITERATIONS);
            for iteration in 0..ITERATIONS {
                let destination = temp
                    .path()
                    .join(format!("d1-run-{label}-{size_mib}mib-{iteration}"));
                let outcome = extract_zip_tier(&archive_path, &destination, tier).unwrap();
                assert_eq!(
                    outcome.logical_bytes, logical,
                    "D1 tier {label} lost bytes at {size_mib} MiB iteration {iteration}"
                );
                fs::remove_dir_all(&destination).unwrap();
                elapsed.push(outcome.elapsed);
                writes.push(outcome.write_calls);
                reads.push(outcome.read_calls);
            }
            emit(
                "D1",
                &format!(
                    "size={size_mib}MiB tier={label} logical={logical} archive_bytes={archive_bytes} iterations={ITERATIONS} ms={:.1} write_calls={} read_calls={}",
                    median_ms(elapsed),
                    median_u64(writes),
                    median_u64(reads),
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// D2: A1 + A2 combination along a synthetic single-tool pipeline
// ---------------------------------------------------------------------------

/// Replica of installer/filesystem.rs copy_tree (regular files only).
fn copy_tree_replica(source_dir: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in WalkDir::new(source_dir).follow_links(false).min_depth(1) {
        let entry = entry.map_err(io::Error::other)?;
        let relative = entry
            .path()
            .strip_prefix(source_dir)
            .map_err(io::Error::other)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PipelineStages {
    download_ms: f64,
    verify_ms: f64,
    extract_ms: f64,
    copy_ms: f64,
}

impl PipelineStages {
    fn total_ms(self) -> f64 {
        self.download_ms + self.verify_ms + self.extract_ms + self.copy_ms
    }
}

struct PipelineRun {
    stages: PipelineStages,
    digest: String,
    manifest: TreeManifest,
}

/// Single-tool pipeline replica: "download" the archive bytes through the
/// production 64 KiB stream loop (with or without the A1 inline hasher),
/// verify (full re-read, or A1 memory finalize), extract (production buffers
/// or the A2 tier), then stage the unpacked tree via copy_tree.
fn d2_run_pipeline(
    archive_path: &Path,
    work: &Path,
    pipeline_hash: bool,
    tier: Option<usize>,
) -> io::Result<PipelineRun> {
    let downloads = work.join("downloads");
    fs::create_dir_all(&downloads)?;
    let target = downloads.join("tool.zip");
    let mut input = File::open(archive_path)?;
    let mut output = File::create(&target)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let download_started = Instant::now();
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if pipeline_hash {
            hasher.update(&buffer[..read]);
        }
        output.write_all(&buffer[..read])?;
    }
    output.sync_all()?;
    drop(output);
    let download_ms = download_started.elapsed().as_secs_f64() * 1000.0;

    let verify_started = Instant::now();
    let digest = if pipeline_hash {
        hex_digest(&hasher.finalize())
    } else {
        verify_sha256_replica(&target)?
    };
    let verify_ms = verify_started.elapsed().as_secs_f64() * 1000.0;

    let unpacked = work.join("unpacked");
    let extract_started = Instant::now();
    extract_zip_tier(&target, &unpacked, tier)?;
    let extract_ms = extract_started.elapsed().as_secs_f64() * 1000.0;

    let staging = work.join("staging").join("content");
    let copy_started = Instant::now();
    copy_tree_replica(&unpacked, &staging)?;
    let copy_ms = copy_started.elapsed().as_secs_f64() * 1000.0;

    Ok(PipelineRun {
        stages: PipelineStages {
            download_ms,
            verify_ms,
            extract_ms,
            copy_ms,
        },
        digest,
        manifest: tree_manifest(&staging)?,
    })
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture --test-threads=1"]
fn d2_pipeline_combination() {
    let temp = tempdir().unwrap();
    let logical = 256 * MIB;
    let archive_path = temp.path().join("d2-256mib.zip");
    build_mixed_zip(
        &PatternSource::new(0xD2CE_0000_0000_0002),
        &archive_path,
        logical,
    )
    .unwrap();
    let expected_digest = verify_sha256_replica(&archive_path).unwrap();

    // (label, pipeline_hash A1, extraction tier A2)
    let variants: [(&str, bool, Option<usize>); 4] = [
        ("baseline", false, None),
        ("a1", true, None),
        ("a2", false, Some(A2_TIER)),
        ("a1+a2", true, Some(A2_TIER)),
    ];
    let mut medians: Vec<(&str, PipelineStages)> = Vec::with_capacity(variants.len());
    let mut baseline_pair: Option<(String, TreeManifest)> = None;
    for (label, pipeline_hash, tier) in variants {
        let mut samples: Vec<PipelineStages> = Vec::with_capacity(ITERATIONS);
        let mut reference: Option<(String, TreeManifest)> = None;
        for iteration in 0..ITERATIONS {
            let work = temp.path().join(format!("d2-{label}-{iteration}"));
            let run = d2_run_pipeline(&archive_path, &work, pipeline_hash, tier).unwrap();
            assert_eq!(
                run.digest, expected_digest,
                "D2 {label} digest drifted at iteration {iteration}"
            );
            if let Some((digest, manifest)) = &reference {
                assert_eq!(
                    &run.digest, digest,
                    "D2 {label} digest drifted at iteration {iteration}"
                );
                assert_eq!(
                    &run.manifest, manifest,
                    "D2 {label} staging tree drifted at iteration {iteration}"
                );
            } else {
                reference = Some((run.digest.clone(), run.manifest.clone()));
            }
            if baseline_pair.is_none() {
                baseline_pair = Some((run.digest.clone(), run.manifest.clone()));
            } else if let Some((digest, manifest)) = &baseline_pair {
                assert_eq!(&run.digest, digest, "D2 {label} tree differs from baseline");
                assert_eq!(
                    &run.manifest, manifest,
                    "D2 {label} staging tree differs from baseline"
                );
            }
            samples.push(run.stages);
            fs::remove_dir_all(&work).unwrap();
        }
        let stages = PipelineStages {
            download_ms: median_f(samples.iter().map(|s| s.download_ms).collect()),
            verify_ms: median_f(samples.iter().map(|s| s.verify_ms).collect()),
            extract_ms: median_f(samples.iter().map(|s| s.extract_ms).collect()),
            copy_ms: median_f(samples.iter().map(|s| s.copy_ms).collect()),
        };
        emit(
            "D2",
            &format!(
                "variant={label} download_ms={:.1} verify_ms={:.3} extract_ms={:.1} copy_ms={:.1} total_ms={:.1}",
                stages.download_ms,
                stages.verify_ms,
                stages.extract_ms,
                stages.copy_ms,
                stages.total_ms(),
            ),
        );
        medians.push((label, stages));
    }
    let baseline = medians[0].1.total_ms();
    let a1 = medians[1].1.total_ms();
    let a2 = medians[2].1.total_ms();
    let both = medians[3].1.total_ms();
    let gain_a1 = baseline - a1;
    let gain_a2 = baseline - a2;
    let gain_both = baseline - both;
    let interaction = gain_both - gain_a1 - gain_a2;
    emit(
        "D2",
        &format!(
            "summary baseline_total_ms={baseline:.1} a1_total_ms={a1:.1} a2_total_ms={a2:.1} both_total_ms={both:.1} gain_a1_ms={gain_a1:.1} gain_a2_ms={gain_a2:.1} gain_both_ms={gain_both:.1} interaction_ms={interaction:.1} (positive = subadditive shortfall, negative = superadditive) gain_both_pct={:.1}",
            gain_both / baseline * 100.0,
        ),
    );
}

// ---------------------------------------------------------------------------
// D3: concurrent extraction with four workers (jobs=4 simulation)
// ---------------------------------------------------------------------------

const D3_WORKERS: usize = 4;

fn d3_round(archive_path: &Path, dest_root: &Path, tier: Option<usize>) -> Duration {
    let started = Instant::now();
    thread::scope(|scope| {
        for worker in 0..D3_WORKERS {
            let destination = dest_root.join(worker.to_string());
            scope.spawn(move || {
                extract_zip_tier(archive_path, &destination, tier).unwrap();
            });
        }
    });
    started.elapsed()
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture --test-threads=1"]
fn d3_concurrent_extraction() {
    let temp = tempdir().unwrap();
    let logical = 256 * MIB;
    let archive_path = temp.path().join("d3-256mib.zip");
    build_mixed_zip(
        &PatternSource::new(0xD3CE_0000_0000_0003),
        &archive_path,
        logical,
    )
    .unwrap();

    // Equivalence: baseline vs tiered trees.
    let base_dir = temp.path().join("d3-eq-base");
    let opt_dir = temp.path().join("d3-eq-opt");
    extract_zip_tier(&archive_path, &base_dir, None).unwrap();
    extract_zip_tier(&archive_path, &opt_dir, Some(A2_TIER)).unwrap();
    assert_eq!(
        tree_manifest(&base_dir).unwrap(),
        tree_manifest(&opt_dir).unwrap(),
        "D3 trees differ"
    );
    fs::remove_dir_all(&base_dir).unwrap();
    fs::remove_dir_all(&opt_dir).unwrap();

    for (label, tier) in [("baseline", None), ("optimized", Some(A2_TIER))] {
        // Warmup round (page cache, allocator), then measured rounds.
        let warmup = temp.path().join("d3-warm");
        d3_round(&archive_path, &warmup, tier);
        fs::remove_dir_all(&warmup).unwrap();
        let mut walls = Vec::with_capacity(ITERATIONS);
        for iteration in 0..ITERATIONS {
            let dest_root = temp.path().join(format!("d3-{label}-{iteration}"));
            let wall = d3_round(&archive_path, &dest_root, tier);
            for worker in 0..D3_WORKERS {
                assert!(
                    dest_root
                        .join(worker.to_string())
                        .join("payload/deflated.bin")
                        .is_file(),
                    "D3 {label} round {iteration} worker {worker} output missing"
                );
            }
            fs::remove_dir_all(&dest_root).unwrap();
            walls.push(wall);
        }
        let wall_ms = median_ms(walls);
        let throughput = (logical * D3_WORKERS as u64) as f64 / MIB as f64 / (wall_ms / 1000.0);
        let per_worker_bytes = tier
            .map(|capacity| 2 * capacity as u64)
            .unwrap_or(3 * 8 * 1024);
        emit(
            "D3",
            &format!(
                "workers={D3_WORKERS} size=256MiB each variant={label} wall_ms={wall_ms:.1} aggregate_mib_s={throughput:.0} theoretical_extra_rss_bytes={}",
                per_worker_bytes * D3_WORKERS as u64,
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// D4: hard_link-first copy_tree pricing and failure branches
// ---------------------------------------------------------------------------

/// Mixed tree: 6 big files (85%), 512 mid files (14%), 256 small files (1%),
/// the first-round B3 shape.
fn build_mixed_tree(source: &PatternSource, root: &Path, size_mib: u64) -> io::Result<usize> {
    let size = size_mib * MIB;
    fs::create_dir_all(root.join("bin"))?;
    fs::create_dir_all(root.join("lib"))?;
    fs::create_dir_all(root.join("etc"))?;
    let big_each = size * 85 / 100 / 6;
    let mid_each = size * 14 / 100 / 512;
    let small_each = (size - size * 85 / 100 - size * 14 / 100) / 256;
    let mut start = 0_u64;
    let mut files = 0_usize;
    for index in 0..6 {
        write_pattern_file(
            source,
            &root.join("bin").join(format!("big-{index:02}.dat")),
            start,
            big_each,
        )?;
        start += big_each;
        files += 1;
    }
    for index in 0..512 {
        write_pattern_file(
            source,
            &root.join("lib").join(format!("mid-{index:03}.dat")),
            start,
            mid_each,
        )?;
        start += mid_each;
        files += 1;
    }
    for index in 0..256 {
        write_pattern_file(
            source,
            &root.join("etc").join(format!("small-{index:03}.dat")),
            start,
            small_each,
        )?;
        start += small_each;
        files += 1;
    }
    Ok(files)
}

fn write_pattern_file(
    source: &PatternSource,
    path: &Path,
    start: u64,
    length: u64,
) -> io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, file);
    let mut reader = PatternReader::new(source, start, length);
    io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    Ok(())
}

#[derive(Default)]
struct HybridStats {
    links: u64,
    fallback_copies: u64,
}

/// A4 candidate replica: link first, fall back to a full copy on any error.
fn hybrid_tree_replica(source_dir: &Path, destination: &Path) -> io::Result<HybridStats> {
    let mut stats = HybridStats::default();
    fs::create_dir_all(destination)?;
    for entry in WalkDir::new(source_dir).follow_links(false).min_depth(1) {
        let entry = entry.map_err(io::Error::other)?;
        let relative = entry
            .path()
            .strip_prefix(source_dir)
            .map_err(io::Error::other)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            match fs::hard_link(entry.path(), &target) {
                Ok(()) => stats.links += 1,
                Err(_) => {
                    fs::copy(entry.path(), &target)?;
                    stats.fallback_copies += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Drive-letter root ("C:\") for a path, or None for UNC/prefixed paths.
#[cfg(windows)]
fn volume_root(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let mut root = String::with_capacity(3);
        root.push(bytes[0] as char);
        root.push(':');
        root.push('\\');
        Some(PathBuf::from(root))
    } else {
        None
    }
}

#[cfg(windows)]
fn volume_serial(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    let root = volume_root(path)?;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_size: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_size: u32,
        ) -> i32;
    }
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
    let mut serial = 0_u32;
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    (ok != 0).then_some(serial as u64)
}

#[cfg(not(windows))]
fn volume_serial(_path: &Path) -> Option<u64> {
    None
}

/// First writable candidate root on a volume different from the temp volume.
fn secondary_volume_root() -> Option<PathBuf> {
    let primary = volume_serial(&std::env::temp_dir())?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        candidates.push(current);
    }
    candidates.push(PathBuf::from("D:\\"));
    candidates.push(PathBuf::from("E:\\"));
    candidates.into_iter().find(|candidate| {
        volume_serial(candidate) != Some(primary)
            && Builder::new()
                .prefix("d4-probe-")
                .tempdir_in(candidate)
                .is_ok()
    })
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture --test-threads=1"]
fn d4_hardlink_fallback_paths() {
    const LATENCY_ATTEMPTS: usize = 2000;
    let temp = tempdir().unwrap();
    let source = PatternSource::new(0xD4CE_0000_0000_0004);
    let tree_root = temp.path().join("d4-src");
    let files = build_mixed_tree(&source, &tree_root, 256).unwrap();
    let source_manifest = tree_manifest(&tree_root).unwrap();

    // Same-volume pair: hybrid == all links, no fallbacks.
    let mut copy_times = Vec::with_capacity(ITERATIONS);
    let mut hybrid_times = Vec::with_capacity(ITERATIONS);
    let mut same_stats = HybridStats::default();
    for iteration in 0..ITERATIONS {
        let destination = temp.path().join(format!("d4-same-copy-{iteration}"));
        let started = Instant::now();
        copy_tree_replica(&tree_root, &destination).unwrap();
        copy_times.push(started.elapsed());
        assert_eq!(
            tree_manifest(&destination).unwrap(),
            source_manifest,
            "D4 same-volume copy tree differs from the source"
        );
        fs::remove_dir_all(&destination).unwrap();

        let destination = temp.path().join(format!("d4-same-hybrid-{iteration}"));
        let started = Instant::now();
        let stats = hybrid_tree_replica(&tree_root, &destination).unwrap();
        hybrid_times.push(started.elapsed());
        if iteration == 0 {
            same_stats = stats;
            assert_eq!(
                tree_manifest(&destination).unwrap(),
                source_manifest,
                "D4 same-volume hybrid tree differs from the source"
            );
        }
        fs::remove_dir_all(&destination).unwrap();
    }
    assert_eq!(
        (same_stats.links, same_stats.fallback_copies),
        (files as u64, 0),
        "same-volume hybrid must link every file"
    );
    let same_copy_ms = median_ms(copy_times);
    let same_hybrid_ms = median_ms(hybrid_times);
    emit(
        "D4",
        &format!(
            "scope=same-volume files={files} copy_ms={same_copy_ms:.1} hybrid_ms={same_hybrid_ms:.1} reduction_pct={:.1}",
            (same_copy_ms - same_hybrid_ms) / same_copy_ms * 100.0,
        ),
    );

    // Cross-volume pair (here C: temp -> D: workspace): hard_link fails with
    // ERROR_NOT_SAME_DEVICE and the hybrid falls back to full copies.
    match secondary_volume_root() {
        Some(root) => {
            let free = volume_free_bytes(&root).unwrap_or(0);
            if free < 2 * 256 * MIB {
                emit(
                    "D4",
                    &format!(
                        "scope=cross-volume skipped: only {free} bytes free on {}",
                        root.display()
                    ),
                );
                return;
            }
            let cross = Builder::new()
                .prefix("d4-cross-")
                .tempdir_in(&root)
                .unwrap();
            let cross_root = cross.path();

            // One explicit probe proving the failure mode is the expected
            // cross-volume error (Windows ERROR_NOT_SAME_DEVICE = 17).
            let probe_target = cross_root.join("probe-link");
            match fs::hard_link(tree_root.join("bin/big-00.dat"), &probe_target) {
                Ok(()) => panic!("cross-volume hard_link unexpectedly succeeded"),
                Err(error) => emit(
                    "D4",
                    &format!(
                        "scope=cross-volume root={} probe_error={error:?} raw_os_error={:?}",
                        root.display(),
                        error.raw_os_error()
                    ),
                ),
            }

            let mut copy_times = Vec::with_capacity(ITERATIONS);
            let mut hybrid_times = Vec::with_capacity(ITERATIONS);
            let mut cross_stats = HybridStats::default();
            for iteration in 0..ITERATIONS {
                let destination = cross_root.join(format!("copy-{iteration}"));
                let started = Instant::now();
                copy_tree_replica(&tree_root, &destination).unwrap();
                copy_times.push(started.elapsed());
                assert_eq!(
                    tree_manifest(&destination).unwrap(),
                    source_manifest,
                    "D4 cross-volume copy tree differs from the source"
                );
                fs::remove_dir_all(&destination).unwrap();

                let destination = cross_root.join(format!("hybrid-{iteration}"));
                let started = Instant::now();
                let stats = hybrid_tree_replica(&tree_root, &destination).unwrap();
                hybrid_times.push(started.elapsed());
                if iteration == 0 {
                    cross_stats = stats;
                    assert_eq!(
                        tree_manifest(&destination).unwrap(),
                        source_manifest,
                        "D4 cross-volume hybrid tree differs from the source"
                    );
                }
                fs::remove_dir_all(&destination).unwrap();
            }
            assert_eq!(
                (cross_stats.links, cross_stats.fallback_copies),
                (0, files as u64),
                "cross-volume hybrid must fall back to copies for every file"
            );
            let cross_copy_ms = median_ms(copy_times);
            let cross_hybrid_ms = median_ms(hybrid_times);
            emit(
                "D4",
                &format!(
                    "scope=cross-volume root={} files={} copy_ms={cross_copy_ms:.1} hybrid_ms={cross_hybrid_ms:.1} fallback_overhead_ms={:.3} fallbacks={}",
                    root.display(),
                    files,
                    cross_hybrid_ms - cross_copy_ms,
                    cross_stats.fallback_copies,
                ),
            );

            // Failed-syscall latency: repeated cross-volume link attempts.
            let started = Instant::now();
            let mut failures = 0_usize;
            for _ in 0..LATENCY_ATTEMPTS {
                if fs::hard_link(tree_root.join("bin/big-00.dat"), &probe_target).is_err() {
                    failures += 1;
                }
            }
            let elapsed = started.elapsed();
            assert_eq!(failures, LATENCY_ATTEMPTS);
            emit(
                "D4",
                &format!(
                    "scope=failed-syscall attempts={LATENCY_ATTEMPTS} total_us={:.0} per_call_us={:.2}",
                    elapsed.as_secs_f64() * 1_000_000.0,
                    elapsed.as_secs_f64() * 1_000_000.0 / LATENCY_ATTEMPTS as f64,
                ),
            );
        }
        None => {
            emit(
                "D4",
                "scope=cross-volume skipped: no second writable volume found (subdirectory simulation would not reproduce ERROR_NOT_SAME_DEVICE)",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// D6: LZMA share sensitivity for the A15 decoder swap
// ---------------------------------------------------------------------------

const METHOD_DEFLATE: u16 = 8;
const METHOD_LZMA: u16 = 14;
const DOS_TIME: u16 = 12 << 11; // 12:00:00
const DOS_DATE: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1; // 2026-01-01

struct RawZipEntry {
    name: String,
    method: u16,
    version_needed: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    offset: u32,
}

/// Minimal ZIP writer for members whose compressed payloads are produced
/// externally (the zip crate cannot write LZMA members, and uniform control
/// over both methods keeps the framing identical for every tier).
struct RawZipWriter {
    out: BufWriter<File>,
    entries: Vec<RawZipEntry>,
    offset: u32,
}

impl RawZipWriter {
    fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            out: BufWriter::with_capacity(256 * 1024, File::create(path)?),
            entries: Vec::new(),
            offset: 0,
        })
    }

    fn add_entry(
        &mut self,
        name: &str,
        method: u16,
        version_needed: u16,
        uncompressed_size: u64,
        data: &[u8],
        crc32: u32,
    ) -> io::Result<()> {
        assert!(
            data.len() <= u32::MAX as usize && uncompressed_size <= u32::MAX as u64,
            "member exceeds zip32"
        );
        let offset = self.offset;
        let mut header = Vec::with_capacity(30 + name.len());
        header.extend_from_slice(&0x0403_4B50_u32.to_le_bytes());
        header.extend_from_slice(&version_needed.to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes()); // flags
        header.extend_from_slice(&method.to_le_bytes());
        header.extend_from_slice(&DOS_TIME.to_le_bytes());
        header.extend_from_slice(&DOS_DATE.to_le_bytes());
        header.extend_from_slice(&crc32.to_le_bytes());
        header.extend_from_slice(&(data.len() as u32).to_le_bytes());
        header.extend_from_slice(&(uncompressed_size as u32).to_le_bytes());
        header.extend_from_slice(&(name.len() as u16).to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes()); // extra length
        header.extend_from_slice(name.as_bytes());
        self.out.write_all(&header)?;
        self.out.write_all(data)?;
        self.offset = offset + (header.len() + data.len()) as u32;
        self.entries.push(RawZipEntry {
            name: name.to_owned(),
            method,
            version_needed,
            crc32,
            compressed_size: data.len() as u32,
            uncompressed_size: uncompressed_size as u32,
            offset,
        });
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        let central_offset = self.offset;
        let mut central = Vec::new();
        for entry in &self.entries {
            central.extend_from_slice(&0x0201_4B50_u32.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes()); // version made by
            central.extend_from_slice(&entry.version_needed.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes()); // flags
            central.extend_from_slice(&entry.method.to_le_bytes());
            central.extend_from_slice(&DOS_TIME.to_le_bytes());
            central.extend_from_slice(&DOS_DATE.to_le_bytes());
            central.extend_from_slice(&entry.crc32.to_le_bytes());
            central.extend_from_slice(&entry.compressed_size.to_le_bytes());
            central.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
            central.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes()); // extra
            central.extend_from_slice(&0_u16.to_le_bytes()); // comment
            central.extend_from_slice(&0_u16.to_le_bytes()); // disk start
            central.extend_from_slice(&0_u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0_u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&entry.offset.to_le_bytes());
            central.extend_from_slice(entry.name.as_bytes());
        }
        let central_size = central.len() as u32;
        self.out.write_all(&central)?;
        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&0x0605_4B50_u32.to_le_bytes());
        eocd.extend_from_slice(&0_u16.to_le_bytes()); // disk number
        eocd.extend_from_slice(&0_u16.to_le_bytes()); // central dir disk
        eocd.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&central_size.to_le_bytes());
        eocd.extend_from_slice(&central_offset.to_le_bytes());
        eocd.extend_from_slice(&0_u16.to_le_bytes()); // comment length
        self.out.write_all(&eocd)?;
        self.out.flush()
    }
}

/// Compress one member either with liblzma LZMA1-alone (reframed into the
/// ZIP-LZMA 4+5 byte header the production parser expects) or with raw
/// flate2 deflate; returns the payload plus the CRC-32 of the logical bytes.
fn encode_member(
    source: &PatternSource,
    start: u64,
    length: u64,
    lzma: bool,
) -> io::Result<(Vec<u8>, u32)> {
    let mut reader = PatternReader::new(source, start, length);
    let mut crc = CrcDigest::new(CrcAlgorithm::Crc32IsoHdlc);
    let mut chunk = vec![0_u8; MIB as usize];
    if lzma {
        let options = LzmaOptions::new_preset(6).map_err(io::Error::other)?;
        let stream = XzStream::new_lzma_encoder(&options).map_err(io::Error::other)?;
        let mut encoder = XzEncoder::new_stream(Vec::new(), stream);
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            crc.update(&chunk[..read]);
            encoder.write_all(&chunk[..read])?;
        }
        let alone = encoder.finish().map_err(io::Error::other)?;
        let mut framed = Vec::with_capacity(alone.len() + 4);
        framed.extend_from_slice(&1_u16.to_le_bytes());
        framed.extend_from_slice(&5_u16.to_le_bytes());
        framed.extend_from_slice(&alone[0..5]);
        framed.extend_from_slice(&alone[13..]);
        Ok((framed, crc.finalize() as u32))
    } else {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            crc.update(&chunk[..read]);
            encoder.write_all(&chunk[..read])?;
        }
        Ok((encoder.finish()?, crc.finalize() as u32))
    }
}

fn build_d6_zip(
    source: &PatternSource,
    path: &Path,
    logical: u64,
    lzma_members: usize,
    total_members: usize,
) -> io::Result<u64> {
    let member_len = logical / total_members as u64;
    let mut writer = RawZipWriter::create(path)?;
    let mut lzma_bytes = 0_u64;
    for index in 0..total_members {
        let start = index as u64 * member_len;
        let (method, version_needed) = if index < lzma_members {
            lzma_bytes += member_len;
            (METHOD_LZMA, 63_u16)
        } else {
            (METHOD_DEFLATE, 20_u16)
        };
        let (payload, crc32) = encode_member(source, start, member_len, index < lzma_members)?;
        writer.add_entry(
            &format!("payload/member-{index:02}.bin"),
            method,
            version_needed,
            member_len,
            &payload,
            crc32,
        )?;
    }
    writer.finish()?;
    Ok(lzma_bytes)
}

/// CRC/size-checking sink mirroring the production CheckedZipWriter (default
/// BufWriter + crc-fast CRC-32 computed over decoded output).
struct CheckedSink<W: Write> {
    inner: BufWriter<W>,
    crc32: CrcDigest,
    written: u64,
}

impl<W: Write> CheckedSink<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: BufWriter::new(inner),
            crc32: CrcDigest::new(CrcAlgorithm::Crc32IsoHdlc),
            written: 0,
        }
    }

    fn finish(mut self) -> io::Result<(u64, u32)> {
        self.inner.flush()?;
        Ok((self.written, self.crc32.finalize() as u32))
    }
}

impl<W: Write> Write for CheckedSink<W> {
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

/// Parses the ZIP-LZMA framing exactly like the production path (property
/// size, packed property byte, literal-context check, dictionary bounds) and
/// returns the five property bytes.
fn parse_zip_lzma_properties(input: &mut impl Read) -> io::Result<[u8; 5]> {
    const MAX_DICTIONARY_SIZE: u32 = 512 * 1024 * 1024;
    let mut header = [0_u8; 4];
    input.read_exact(&mut header)?;
    let property_size = u16::from_le_bytes([header[2], header[3]]);
    if property_size != 5 {
        return Err(io::Error::other(format!(
            "unsupported ZIP LZMA property size {property_size}"
        )));
    }
    let mut properties = [0_u8; 5];
    input.read_exact(&mut properties)?;
    let mut packed = u32::from(properties[0]);
    if packed >= 225 {
        return Err(io::Error::other(format!(
            "invalid ZIP LZMA property byte {}",
            properties[0]
        )));
    }
    let lc = packed % 9;
    packed /= 9;
    let lp = packed % 5;
    let _ = packed / 5; // pb
    if lc + lp > 4 {
        return Err(io::Error::other(format!(
            "invalid ZIP LZMA literal properties lc={lc}, lp={lp}"
        )));
    }
    let dictionary_size =
        u32::from_le_bytes(properties[1..5].try_into().expect("four bytes")).max(4 * 1024);
    if dictionary_size > MAX_DICTIONARY_SIZE {
        return Err(io::Error::other(
            "ZIP LZMA dictionary size exceeds the safety limit",
        ));
    }
    Ok(properties)
}

/// Baseline LZMA decode replica: lzma-rs raw LzmaDecoder over the remaining
/// entry bytes (production extract_zip_lzma shape).
fn d6_decode_lzma_rs(
    input: &mut impl Read,
    mut output: &mut dyn Write,
    unpacked_size: u64,
) -> io::Result<()> {
    const MAX_DICTIONARY_SIZE: u32 = 512 * 1024 * 1024;
    let properties = parse_zip_lzma_properties(input)?;
    let dictionary_size =
        u32::from_le_bytes(properties[1..5].try_into().expect("four bytes")).max(4 * 1024);
    let mut packed = u32::from(properties[0]);
    let lc = packed % 9;
    packed /= 9;
    let lp = packed % 5;
    let pb = packed / 5;
    let params = LzmaParams::new(
        LzmaProperties { lc, lp, pb },
        dictionary_size,
        Some(unpacked_size),
    );
    let mut decoder = LzmaDecoder::new(params, Some(MAX_DICTIONARY_SIZE as usize))
        .map_err(|error| io::Error::other(error.to_string()))?;
    decoder
        .decompress(&mut BufReader::new(input), &mut output)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// Reader serving a fixed prefix before delegating to the inner reader: lets
/// the xz2 alone decoder see a contiguous 13-byte header (5 property bytes +
/// 8-byte unpacked size) rebuilt from the consumed ZIP framing.
struct PrefixedReader<R> {
    prefix: Vec<u8>,
    position: usize,
    inner: R,
}

impl<R: Read> PrefixedReader<R> {
    fn new(prefix: Vec<u8>, inner: R) -> Self {
        Self {
            prefix,
            position: 0,
            inner,
        }
    }
}

impl<R: Read> Read for PrefixedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position < self.prefix.len() {
            let count = (self.prefix.len() - self.position).min(buffer.len());
            buffer[..count].copy_from_slice(&self.prefix[self.position..self.position + count]);
            self.position += count;
            return Ok(count);
        }
        self.inner.read(buffer)
    }
}

/// Optimized LZMA decode (A15 candidate): xz2/liblzma LZMA1-alone decoder
/// over the same framed payload with the alone header rebuilt in memory.
/// The rebuilt size field stays u64::MAX ("unknown"): the xz2 alone encoder
/// emits an end-of-stream marker and declares an unknown size, so a known
/// size would make liblzma reject the trailing marker bytes. The unpacked
/// size itself is enforced by the caller's CheckedSink comparison.
fn d6_decode_xz2(
    input: &mut impl Read,
    output: &mut dyn Write,
    _unpacked_size: u64,
) -> io::Result<()> {
    let properties = parse_zip_lzma_properties(input)?;
    let mut prefix = Vec::with_capacity(13);
    prefix.extend_from_slice(&properties);
    prefix.extend_from_slice(&u64::MAX.to_le_bytes());
    let stream = XzStream::new_lzma_decoder(u64::MAX).map_err(io::Error::other)?;
    let mut decoder = XzDecoder::new_stream(PrefixedReader::new(prefix, input), stream);
    io::copy(&mut decoder, output)?;
    Ok(())
}

struct D6Outcome {
    elapsed: Duration,
    lzma: Duration,
    logical: u64,
}

/// Full extraction replica of the production zip() loop with the LZMA branch
/// dispatched through either decoder; the non-LZMA branch keeps the exact
/// production shape (bare File + io::copy, default 8 KiB reads).
fn d6_extract(archive_path: &Path, destination: &Path, liblzma: bool) -> io::Result<D6Outcome> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file)).map_err(io::Error::other)?;
    fs::create_dir_all(destination)?;
    let mut logical = 0_u64;
    let mut lzma_time = Duration::ZERO;
    let started = Instant::now();
    for index in 0..archive.len() {
        let (compression, size, crc32) = {
            let entry = archive.by_index_raw(index).map_err(io::Error::other)?;
            (entry.compression(), entry.size(), entry.crc32())
        };
        if compression == CompressionMethod::Lzma {
            let mut entry = archive.by_index_raw(index).map_err(io::Error::other)?;
            let output = destination.join(entry.name());
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let sink = File::create(&output)?;
            let mut checked = CheckedSink::new(sink);
            let lzma_started = Instant::now();
            let decoded = if liblzma {
                d6_decode_xz2(&mut entry, &mut checked, size)
            } else {
                d6_decode_lzma_rs(&mut entry, &mut checked, size)
            };
            lzma_time += lzma_started.elapsed();
            decoded?;
            let (written, actual_crc32) = checked.finish()?;
            if written != size {
                return Err(io::Error::other(format!(
                    "decompressed {written} bytes, expected {size}"
                )));
            }
            if actual_crc32 != crc32 {
                return Err(io::Error::other(format!(
                    "CRC-32 mismatch: expected {crc32:08x}, got {actual_crc32:08x}"
                )));
            }
            logical += written;
            continue;
        }
        let mut entry = archive.by_index(index).map_err(io::Error::other)?;
        let output = destination.join(entry.name());
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut sink = File::create(&output)?;
        let copied = io::copy(&mut entry, &mut sink)?;
        sink.flush()?;
        assert_eq!(copied, size);
        logical += copied;
    }
    Ok(D6Outcome {
        elapsed: started.elapsed(),
        lzma: lzma_time,
        logical,
    })
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture --test-threads=1"]
fn d6_lzma_share_sensitivity() {
    const TOTAL_MEMBERS: usize = 8;
    let temp = tempdir().unwrap();
    let logical = 256 * MIB;
    let source = PatternSource::new(0xD6CE_0000_0000_0006);
    // 0 LZMA members is the control (both variants take the same path).
    for lzma_members in [0_usize, 1, 4, 8] {
        let archive_path = temp
            .path()
            .join(format!("d6-lzma{lzma_members}-256mib.zip"));
        let lzma_bytes =
            build_d6_zip(&source, &archive_path, logical, lzma_members, TOTAL_MEMBERS).unwrap();
        let archive_bytes = fs::metadata(&archive_path).unwrap().len();
        let fraction = lzma_bytes as f64 / logical as f64;

        // Equivalence: both decoders must reproduce the same tree (the ZIP
        // CRC-32/size double-check already pins it to the generator payload).
        let base_dir = temp.path().join(format!("d6-eq-base-{lzma_members}"));
        let opt_dir = temp.path().join(format!("d6-eq-opt-{lzma_members}"));
        let base_eq = d6_extract(&archive_path, &base_dir, false).unwrap();
        let opt_eq = d6_extract(&archive_path, &opt_dir, true).unwrap();
        assert_eq!(base_eq.logical, logical, "D6 baseline lost bytes");
        assert_eq!(opt_eq.logical, logical, "D6 optimized lost bytes");
        assert_eq!(
            tree_manifest(&base_dir).unwrap(),
            tree_manifest(&opt_dir).unwrap(),
            "D6 trees differ at lzma_members={lzma_members}"
        );
        fs::remove_dir_all(&base_dir).unwrap();
        fs::remove_dir_all(&opt_dir).unwrap();

        let mut base_times = Vec::with_capacity(ITERATIONS);
        let mut opt_times = Vec::with_capacity(ITERATIONS);
        let mut base_lzma = Vec::with_capacity(ITERATIONS);
        let mut opt_lzma = Vec::with_capacity(ITERATIONS);
        for iteration in 0..ITERATIONS {
            let destination = temp
                .path()
                .join(format!("d6-run-base-{lzma_members}-{iteration}"));
            let outcome = d6_extract(&archive_path, &destination, false).unwrap();
            assert_eq!(
                outcome.logical, logical,
                "D6 baseline iteration {iteration}"
            );
            fs::remove_dir_all(&destination).unwrap();
            base_times.push(outcome.elapsed);
            base_lzma.push(outcome.lzma);

            let destination = temp
                .path()
                .join(format!("d6-run-opt-{lzma_members}-{iteration}"));
            let outcome = d6_extract(&archive_path, &destination, true).unwrap();
            assert_eq!(
                outcome.logical, logical,
                "D6 optimized iteration {iteration}"
            );
            fs::remove_dir_all(&destination).unwrap();
            opt_times.push(outcome.elapsed);
            opt_lzma.push(outcome.lzma);
        }
        let base_ms = median_ms(base_times);
        let opt_ms = median_ms(opt_times);
        emit(
            "D6",
            &format!(
                "logical=256MiB lzma_members={lzma_members}/{TOTAL_MEMBERS} lzma_fraction={fraction:.3} archive_bytes={archive_bytes} iterations={ITERATIONS} baseline_ms={base_ms:.1} optimized_ms={opt_ms:.1} overall_speedup={:.2}x baseline_lzma_ms={:.1} optimized_lzma_ms={:.1} total_reduction_pct={:.1}",
                base_ms / opt_ms,
                median_ms(base_lzma),
                median_ms(opt_lzma),
                (base_ms - opt_ms) / base_ms * 100.0,
            ),
        );
        fs::remove_file(&archive_path).unwrap();
    }
}
