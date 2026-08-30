//! Benchmark suite backing the algorithm audit (docs/algorithms-audit-report.md,
//! items B1-B4 / A5 / A7 / A8).
//!
//! Every benchmark runs a faithful replica of the current production code path
//! against an in-benchmark optimized variant on identical inputs. Equivalence
//! (digest / tree manifest / match counts) is asserted before any timing is
//! collected. Timings are medians of `ITERATIONS` runs taken with
//! std::time::Instant. Input tiers are small/medium/large where the large tier
//! is 1 GiB, downgraded to 512 MiB when the temp volume is low on space.
//!
//! Production code is intentionally NOT modified: the optimized variants live
//! here as controlled counterparts only.
//!
//! All tests carry #[ignore] so the regular `cargo test` gate stays fast. Run:
//!     cargo test --release --test algorithm_benchmarks -- --ignored --nocapture

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use lzma_rs::decompress::raw::{LzmaDecoder, LzmaParams, LzmaProperties};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
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

fn emit(tag: &str, message: &str) {
    println!("[{tag}] {message}");
}

fn median_ms(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

fn median_us(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1_000_000.0
}

fn median_u64(mut values: Vec<u64>) -> u64 {
    values.sort();
    values[values.len() / 2]
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
/// adjacent blocks stay mostly identical (so deflate/LZMA see a realistic
/// match structure like real archives) yet every block hashes differently.
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

/// Reader simulating a response body / payload generator: serves `length`
/// bytes of pattern data starting at `start`, 1 MiB per refill.
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

/// Write sink hashing everything that flows through it: equivalence checks
/// without disk interference.
#[derive(Default)]
struct HashingSink {
    hasher: Sha256,
    bytes: u64,
}

impl HashingSink {
    fn finish_hex(&mut self) -> String {
        hex_digest(&self.hasher.clone().finalize())
    }
}

impl Write for HashingSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.hasher.update(buffer);
        self.bytes += buffer.len() as u64;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
fn tree_manifest(root: &Path) -> io::Result<Vec<(String, u64, String)>> {
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

/// Free bytes on the volume holding `path` (used for the large-tier downgrade
/// decision and the B3 physical-usage delta).
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
/// source tree plus archive plus extracted copies, otherwise 512 MiB.
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
// B1: download/hash pipelining (audit hypothesis A1)
// ---------------------------------------------------------------------------

/// Baseline replicates the production split: stream_response writes the body,
/// then finalize/verify_sha256 re-opens the file and hashes all of it. The
/// optimized variant feeds the hasher inside the existing write loop so the
/// verification phase reduces to a finalize over in-memory state.
#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn b1_download_hash_pipeline() {
    let temp = tempdir().unwrap();
    let large_mib = resolve_large_mib(temp.path());
    let source = PatternSource::new(0x5EED_0000_0000_0001);
    for &size_mib in &[64_u64, 256, large_mib] {
        let size = size_mib * MIB;
        let mut baseline_write = Vec::with_capacity(ITERATIONS);
        let mut baseline_verify = Vec::with_capacity(ITERATIONS);
        let mut baseline_total = Vec::with_capacity(ITERATIONS);
        let mut optimized_write = Vec::with_capacity(ITERATIONS);
        let mut optimized_verify = Vec::with_capacity(ITERATIONS);
        let mut optimized_total = Vec::with_capacity(ITERATIONS);
        let mut expected = String::new();
        for iteration in 0..ITERATIONS {
            let path = temp
                .path()
                .join(format!("b1-baseline-{size_mib}mib-{iteration}.part"));
            let (write, verify, digest) = b1_baseline(&source, size, &path).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().len(),
                size,
                "baseline transfer truncated at {size_mib} MiB iteration {iteration}"
            );
            fs::remove_file(&path).unwrap();
            if iteration == 0 {
                expected.clone_from(&digest);
            }
            assert_eq!(
                digest, expected,
                "baseline digest drifted at {size_mib} MiB iteration {iteration}"
            );
            baseline_write.push(write);
            baseline_verify.push(verify);
            baseline_total.push(write + verify);

            let path = temp
                .path()
                .join(format!("b1-optimized-{size_mib}mib-{iteration}.part"));
            let (write, verify, digest) = b1_optimized(&source, size, &path).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().len(),
                size,
                "optimized transfer truncated at {size_mib} MiB iteration {iteration}"
            );
            fs::remove_file(&path).unwrap();
            assert_eq!(
                digest, expected,
                "optimized digest mismatch at {size_mib} MiB iteration {iteration}"
            );
            optimized_write.push(write);
            optimized_verify.push(verify);
            optimized_total.push(write + verify);
        }
        let baseline_write_ms = median_ms(baseline_write);
        let baseline_verify_ms = median_ms(baseline_verify);
        let baseline_total_ms = median_ms(baseline_total);
        let optimized_write_ms = median_ms(optimized_write);
        let optimized_verify_ms = median_ms(optimized_verify);
        let optimized_total_ms = median_ms(optimized_total);
        let verify_reduction =
            (baseline_verify_ms - optimized_verify_ms) / baseline_verify_ms * 100.0;
        let total_reduction = (baseline_total_ms - optimized_total_ms) / baseline_total_ms * 100.0;
        emit(
            "B1",
            &format!(
                "size={size_mib}MiB iterations={ITERATIONS} baseline_write_ms={baseline_write_ms:.1} baseline_verify_ms={baseline_verify_ms:.1} baseline_total_ms={baseline_total_ms:.1} optimized_write_ms={optimized_write_ms:.1} optimized_verify_ms={optimized_verify_ms:.3} optimized_total_ms={optimized_total_ms:.1} verify_reduction_pct={verify_reduction:.1} total_reduction_pct={total_reduction:.1} verify_read_bytes_baseline={size} verify_read_bytes_optimized=0"
            ),
        );
    }
}

/// Replica of stream_response (64 KiB read/write_all loop + sync_all) followed
/// by a full re-read hash, mirroring finalize's verify_sha256 call.
fn b1_baseline(
    source: &PatternSource,
    size: u64,
    path: &Path,
) -> io::Result<(Duration, Duration, String)> {
    let mut file = File::create(path)?;
    let mut reader = PatternReader::new(source, 0, size);
    let mut buffer = vec![0_u8; 64 * 1024];
    let write_started = Instant::now();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
    }
    file.sync_all()?;
    let write_elapsed = write_started.elapsed();

    let verify_started = Instant::now();
    let digest = verify_sha256_replica(path)?;
    Ok((write_elapsed, verify_started.elapsed(), digest))
}

/// Optimized variant: the hasher rides the existing write loop, verification
/// is a finalize over memory state only (zero extra reads).
fn b1_optimized(
    source: &PatternSource,
    size: u64,
    path: &Path,
) -> io::Result<(Duration, Duration, String)> {
    let mut file = File::create(path)?;
    let mut reader = PatternReader::new(source, 0, size);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let write_started = Instant::now();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    file.sync_all()?;
    let write_elapsed = write_started.elapsed();

    let verify_started = Instant::now();
    let digest = hex_digest(&hasher.finalize());
    Ok((write_elapsed, verify_started.elapsed(), digest))
}

// ---------------------------------------------------------------------------
// B2: extraction I/O buffering (audit hypothesis A2)
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
    write_bytes: u64,
    read_calls: u64,
}

fn build_b2_zip(
    source: &PatternSource,
    path: &Path,
    deflate_len: u64,
    stored_len: u64,
) -> io::Result<()> {
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

/// Baseline replica of the production zip() loop: default 8 KiB BufReader on
/// the archive, bare File sink behind the CountingWriter, std io::copy with
/// its default 8 KiB buffer.
fn extract_zip_baseline(archive_path: &Path, destination: &Path) -> io::Result<ExtractOutcome> {
    let counters = IoCounters::default();
    let reader = CountingFileReader::open(archive_path, counters.clone())?;
    let mut archive = ZipArchive::new(BufReader::new(reader)).map_err(io::Error::other)?;
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
        let sink = CountingFileWriter::create(&output, counters.clone())?;
        let mut limited = CountingSink::new(sink);
        io::copy(&mut entry, &mut limited)?;
        logical += limited.written();
        assert_eq!(limited.written(), size);
    }
    let elapsed = started.elapsed();
    Ok(ExtractOutcome {
        elapsed,
        logical_bytes: logical,
        write_calls: counters.write_calls.get(),
        write_bytes: counters.write_bytes.get(),
        read_calls: counters.read_calls.get(),
    })
}

/// Optimized variant: 256 KiB read buffering plus a BufWriter of
/// `write_capacity` in front of the same counting file sink.
fn extract_zip_optimized(
    archive_path: &Path,
    destination: &Path,
    write_capacity: usize,
) -> io::Result<ExtractOutcome> {
    let counters = IoCounters::default();
    let reader = CountingFileReader::open(archive_path, counters.clone())?;
    let mut archive =
        ZipArchive::new(BufReader::with_capacity(256 * 1024, reader)).map_err(io::Error::other)?;
    fs::create_dir_all(destination)?;
    let mut logical = 0_u64;
    let started = Instant::now();
    for index in 0..archive.len() {
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
        let sink = CountingFileWriter::create(&output, counters.clone())?;
        let mut limited = CountingSink::new(BufWriter::with_capacity(write_capacity, sink));
        io::copy(&mut entry, &mut limited)?;
        // Drain the BufWriter so the counters reflect syscall state.
        limited.flush()?;
        logical += limited.written();
        assert_eq!(limited.written(), size);
    }
    let elapsed = started.elapsed();
    Ok(ExtractOutcome {
        elapsed,
        logical_bytes: logical,
        write_calls: counters.write_calls.get(),
        write_bytes: counters.write_bytes.get(),
        read_calls: counters.read_calls.get(),
    })
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn b2_extract_io_buffers() {
    let temp = tempdir().unwrap();
    let large_mib = resolve_large_mib(temp.path());
    let source = PatternSource::new(0x5EED_0000_0000_0002);
    for &size_mib in &[64_u64, 256, large_mib] {
        let logical = size_mib * MIB;
        let deflate_len = logical * 4 / 5;
        let stored_len = logical - deflate_len;
        let archive_path = temp.path().join(format!("b2-{size_mib}mib.zip"));
        build_b2_zip(&source, &archive_path, deflate_len, stored_len).unwrap();
        let archive_bytes = fs::metadata(&archive_path).unwrap().len();

        // Equivalence pass: identical output trees for both versions.
        let base_dir = temp.path().join(format!("b2-eq-base-{size_mib}mib"));
        let opt_dir = temp.path().join(format!("b2-eq-opt-{size_mib}mib"));
        let base_outcome = extract_zip_baseline(&archive_path, &base_dir).unwrap();
        let opt_outcome = extract_zip_optimized(&archive_path, &opt_dir, 256 * 1024).unwrap();
        assert_eq!(
            tree_manifest(&base_dir).unwrap(),
            tree_manifest(&opt_dir).unwrap(),
            "B2 output trees differ at {size_mib} MiB"
        );
        assert_eq!(base_outcome.logical_bytes, logical);
        assert_eq!(opt_outcome.logical_bytes, logical);
        fs::remove_dir_all(&base_dir).unwrap();
        fs::remove_dir_all(&opt_dir).unwrap();

        let mut base_runs = Vec::with_capacity(ITERATIONS);
        let mut opt64_runs = Vec::with_capacity(ITERATIONS);
        let mut opt256_runs = Vec::with_capacity(ITERATIONS);
        for iteration in 0..ITERATIONS {
            let destination = temp
                .path()
                .join(format!("b2-run-base-{size_mib}mib-{iteration}"));
            let outcome = extract_zip_baseline(&archive_path, &destination).unwrap();
            fs::remove_dir_all(&destination).unwrap();
            base_runs.push(outcome);

            let destination = temp
                .path()
                .join(format!("b2-run-opt64-{size_mib}mib-{iteration}"));
            let outcome = extract_zip_optimized(&archive_path, &destination, 64 * 1024).unwrap();
            fs::remove_dir_all(&destination).unwrap();
            opt64_runs.push(outcome);

            let destination = temp
                .path()
                .join(format!("b2-run-opt256-{size_mib}mib-{iteration}"));
            let outcome = extract_zip_optimized(&archive_path, &destination, 256 * 1024).unwrap();
            fs::remove_dir_all(&destination).unwrap();
            opt256_runs.push(outcome);
        }
        let base_ms = median_ms(base_runs.iter().map(|run| run.elapsed).collect());
        let opt64_ms = median_ms(opt64_runs.iter().map(|run| run.elapsed).collect());
        let opt256_ms = median_ms(opt256_runs.iter().map(|run| run.elapsed).collect());
        let base_writes = median_u64(base_runs.iter().map(|run| run.write_calls).collect());
        let opt64_writes = median_u64(opt64_runs.iter().map(|run| run.write_calls).collect());
        let opt256_writes = median_u64(opt256_runs.iter().map(|run| run.write_calls).collect());
        let base_reads = median_u64(base_runs.iter().map(|run| run.read_calls).collect());
        let opt256_reads = median_u64(opt256_runs.iter().map(|run| run.read_calls).collect());
        let base_write_bytes = median_u64(base_runs.iter().map(|run| run.write_bytes).collect());
        let opt256_write_bytes =
            median_u64(opt256_runs.iter().map(|run| run.write_bytes).collect());
        let total_reduction = (base_ms - opt256_ms) / base_ms * 100.0;
        emit(
            "B2",
            &format!(
                "size={size_mib}MiB archive_bytes={archive_bytes} logical_bytes={logical} iterations={ITERATIONS} baseline_ms={base_ms:.1} opt64k_ms={opt64_ms:.1} opt256k_ms={opt256_ms:.1} total_reduction_pct={total_reduction:.1} baseline_write_calls={base_writes} opt64k_write_calls={opt64_writes} opt256k_write_calls={opt256_writes} write_call_reduction={:.1}x baseline_read_calls={base_reads} opt256k_read_calls={opt256_reads} read_call_reduction={:.1}x baseline_write_bytes={base_write_bytes} opt256k_write_bytes={opt256_write_bytes}",
                base_writes as f64 / opt256_writes as f64,
                base_reads as f64 / opt256_reads.max(1) as f64,
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// B3: copy_tree same-volume hardlinking (audit hypothesis A4)
// ---------------------------------------------------------------------------

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

/// Mixed tree: 6 big files (85%), 512 mid files (14%), 256 small files (1%).
fn build_b3_tree(source: &PatternSource, root: &Path, size_mib: u64) -> io::Result<(usize, u64)> {
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
    Ok((files, start))
}

/// Replica of installer/filesystem.rs copy_tree (symlink branch omitted:
/// benchmark trees hold regular files only).
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

/// Optimized variant: identical walk, fs::hard_link instead of fs::copy
/// (same-volume benchmark environment, the fallback path never triggers).
fn hardlink_tree_replica(source_dir: &Path, destination: &Path) -> io::Result<()> {
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
            fs::hard_link(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn b3_copy_tree_hardlinks() {
    let temp = tempdir().unwrap();
    let large_mib = resolve_large_mib(temp.path());
    let source = PatternSource::new(0x5EED_0000_0000_0003);
    for &size_mib in &[64_u64, 256, large_mib] {
        let tree_root = temp.path().join(format!("b3-src-{size_mib}mib"));
        let (files, logical) = build_b3_tree(&source, &tree_root, size_mib).unwrap();
        let source_manifest = tree_manifest(&tree_root).unwrap();

        // Equivalence pass; disk usage is expressed as file count + logical
        // bytes (du semantics: hardlinked trees share clusters, physical
        // footprint stays near zero while logical bytes stay identical).
        let copy_dir = temp.path().join(format!("b3-eq-copy-{size_mib}mib"));
        let link_dir = temp.path().join(format!("b3-eq-link-{size_mib}mib"));
        copy_tree_replica(&tree_root, &copy_dir).unwrap();
        hardlink_tree_replica(&tree_root, &link_dir).unwrap();
        let copy_manifest = tree_manifest(&copy_dir).unwrap();
        let link_manifest = tree_manifest(&link_dir).unwrap();
        assert_eq!(
            source_manifest, copy_manifest,
            "B3 copied tree differs at {size_mib} MiB"
        );
        assert_eq!(
            copy_manifest, link_manifest,
            "B3 linked tree differs at {size_mib} MiB"
        );
        fs::remove_dir_all(&copy_dir).unwrap();
        fs::remove_dir_all(&link_dir).unwrap();

        let mut copy_times = Vec::with_capacity(ITERATIONS);
        let mut link_times = Vec::with_capacity(ITERATIONS);
        for iteration in 0..ITERATIONS {
            let destination = temp
                .path()
                .join(format!("b3-run-copy-{size_mib}mib-{iteration}"));
            let started = Instant::now();
            copy_tree_replica(&tree_root, &destination).unwrap();
            copy_times.push(started.elapsed());
            fs::remove_dir_all(&destination).unwrap();

            let destination = temp
                .path()
                .join(format!("b3-run-link-{size_mib}mib-{iteration}"));
            let started = Instant::now();
            hardlink_tree_replica(&tree_root, &destination).unwrap();
            link_times.push(started.elapsed());
            fs::remove_dir_all(&destination).unwrap();
        }
        let copy_ms = median_ms(copy_times);
        let link_ms = median_ms(link_times);
        let reduction = (copy_ms - link_ms) / copy_ms * 100.0;
        emit(
            "B3",
            &format!(
                "size={size_mib}MiB files={files} logical_bytes={logical} iterations={ITERATIONS} copy_ms={copy_ms:.1} hardlink_ms={link_ms:.1} reduction_pct={reduction:.1} note=logical_bytes_identical_hardlinks_share_clusters"
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// B4: ZIP LZMA decoder swap, lzma-rs vs liblzma/xz2 (audit hypothesis A15)
// ---------------------------------------------------------------------------

/// Baseline replica of archive/extract.rs extract_zip_lzma: parse the ZIP-LZMA
/// framing, decode with lzma-rs raw LzmaDecoder, hash the output stream
/// (standing in for the CRC-checked disk writer).
fn b4_decode_lzma_rs(framed: &[u8], unpacked_size: u64) -> io::Result<(String, u64)> {
    const MAX_DICTIONARY_SIZE: u32 = 512 * 1024 * 1024;

    let mut input = framed;
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
    let pb = packed / 5;
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
    let params = LzmaParams::new(
        LzmaProperties { lc, lp, pb },
        dictionary_size,
        Some(unpacked_size),
    );
    let mut decoder = LzmaDecoder::new(params, Some(MAX_DICTIONARY_SIZE as usize))
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut sink = HashingSink::default();
    let mut buffered = BufReader::new(input);
    decoder
        .decompress(&mut buffered, &mut sink)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let bytes = sink.bytes;
    let digest = sink.finish_hex();
    if bytes != unpacked_size {
        return Err(io::Error::other(format!(
            "decompressed {bytes} bytes, expected {unpacked_size}"
        )));
    }
    Ok((digest, bytes))
}

/// Optimized variant: xz2/liblzma alone-format LZMA1 decoder over the same
/// raw stream (the 13-byte alone header carries the same property byte and
/// dictionary size the ZIP framing would).
fn b4_decode_xz2(alone: &[u8]) -> io::Result<(String, u64)> {
    let stream = XzStream::new_lzma_decoder(u64::MAX)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut decoder = XzDecoder::new_stream(alone, stream);
    let mut sink = HashingSink::default();
    io::copy(&mut decoder, &mut sink)?;
    let bytes = sink.bytes;
    let digest = sink.finish_hex();
    Ok((digest, bytes))
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn b4_lzma_raw_decoder() {
    let source = PatternSource::new(0x5EED_0000_0000_0004);
    for &size_mib in &[64_u64, 256, 1024] {
        let logical = size_mib * MIB;

        // One-time compression: liblzma standalone .lzma stream = 13-byte
        // header (property byte + dict size + size field) + raw LZMA1 payload.
        let options = LzmaOptions::new_preset(6).unwrap();
        let stream = XzStream::new_lzma_encoder(&options).unwrap();
        let mut encoder = XzEncoder::new_stream(Vec::new(), stream);
        let mut payload = PatternReader::new(&source, 0, logical);
        io::copy(&mut payload, &mut encoder).unwrap();
        let alone = encoder.finish().unwrap();
        let compressed_len = alone.len() as u64;

        // Re-frame the raw payload behind the ZIP-LZMA header the production
        // path parses (version, property size, property byte, dict size LE).
        let mut framed = Vec::with_capacity(alone.len() + 4);
        framed.extend_from_slice(&1_u16.to_le_bytes());
        framed.extend_from_slice(&5_u16.to_le_bytes());
        framed.extend_from_slice(&alone[0..5]);
        framed.extend_from_slice(&alone[13..]);

        // Equivalence: both decoders must reproduce the generator output.
        let mut expected = HashingSink::default();
        let mut payload = PatternReader::new(&source, 0, logical);
        io::copy(&mut payload, &mut expected).unwrap();
        let expected_hex = expected.finish_hex();
        let (baseline_hex, baseline_bytes) = b4_decode_lzma_rs(&framed, logical).unwrap();
        let (optimized_hex, optimized_bytes) = b4_decode_xz2(&alone).unwrap();
        assert_eq!(
            baseline_bytes, logical,
            "lzma-rs size mismatch at {size_mib} MiB"
        );
        assert_eq!(
            optimized_bytes, logical,
            "xz2 size mismatch at {size_mib} MiB"
        );
        assert_eq!(
            baseline_hex, expected_hex,
            "lzma-rs decode mismatch at {size_mib} MiB"
        );
        assert_eq!(
            optimized_hex, expected_hex,
            "xz2 decode mismatch at {size_mib} MiB"
        );

        // Timing: pure decode throughput against a hashing sink (no disk).
        let mut baseline_times = Vec::with_capacity(ITERATIONS);
        let mut optimized_times = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let started = Instant::now();
            b4_decode_lzma_rs(&framed, logical).unwrap();
            baseline_times.push(started.elapsed());
            let started = Instant::now();
            b4_decode_xz2(&alone).unwrap();
            optimized_times.push(started.elapsed());
        }
        let baseline_ms = median_ms(baseline_times);
        let optimized_ms = median_ms(optimized_times);
        let baseline_mibs = size_mib as f64 / (baseline_ms / 1000.0);
        let optimized_mibs = size_mib as f64 / (optimized_ms / 1000.0);
        let speedup = baseline_ms / optimized_ms;
        emit(
            "B4",
            &format!(
                "size={size_mib}MiB compressed_bytes={compressed_len} ratio={:.2}x iterations={ITERATIONS} lzma_rs_ms={baseline_ms:.1} xz2_ms={optimized_ms:.1} lzma_rs_mib_s={baseline_mibs:.1} xz2_mib_s={optimized_mibs:.1} speedup={speedup:.2}x",
                logical as f64 / compressed_len as f64,
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// A5: python probe memoization (audit adoption, spawn-count extrapolation)
// ---------------------------------------------------------------------------

fn spawn_probe_replica() -> bool {
    // Models hooks/python.rs probe(): a child process per probe; `cmd /c
    // exit 0` isolates the Windows process-creation cost.
    Command::new("cmd")
        .args(["/c", "exit", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap()
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn a5_python_probe_memoization() {
    const PROBES: usize = 20;
    const ROUNDS: usize = 3;

    let mut baseline_times = Vec::with_capacity(ROUNDS);
    let mut optimized_times = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        // Baseline: every probe spawns a child process.
        let started = Instant::now();
        let mut baseline_spawns = 0_u64;
        for _ in 0..PROBES {
            assert!(spawn_probe_replica());
            baseline_spawns += 1;
        }
        baseline_times.push(started.elapsed());

        // Optimized: the first probe resolves once, later probes hit the
        // per-run memo.
        let started = Instant::now();
        let mut optimized_spawns = 0_u64;
        let mut memoized: Option<bool> = None;
        for _ in 0..PROBES {
            if memoized.is_none() {
                memoized = Some(spawn_probe_replica());
                optimized_spawns += 1;
            }
            assert_eq!(memoized, Some(true));
        }
        optimized_times.push(started.elapsed());

        assert_eq!(baseline_spawns, PROBES as u64);
        assert_eq!(optimized_spawns, 1);
    }
    let baseline_ms = median_ms(baseline_times);
    let optimized_ms = median_ms(optimized_times);
    let savings = (baseline_ms - optimized_ms) / baseline_ms * 100.0;
    let per_probe_ms = baseline_ms / PROBES as f64;
    emit(
        "A5",
        &format!(
            "probes={PROBES} rounds={ROUNDS} baseline_ms={baseline_ms:.1} optimized_ms={optimized_ms:.2} savings_pct={savings:.1} per_probe_ms={per_probe_ms:.1} spawns_baseline={PROBES} spawns_optimized=1 extrapolated_50_hooks_ms_baseline={:.0} extrapolated_50_hooks_ms_optimized={:.0}",
            per_probe_ms * 50.0,
            per_probe_ms,
        ),
    );
}

// ---------------------------------------------------------------------------
// A7: ignore list Vec::contains vs HashSet (audit rejection spot-check)
// ---------------------------------------------------------------------------

fn count_vec_matches(ignored: &[String], candidates: &[String]) -> usize {
    candidates
        .iter()
        .filter(|candidate| ignored.contains(*candidate))
        .count()
}

fn count_set_matches(ignored: &HashSet<String>, candidates: &[String]) -> usize {
    candidates
        .iter()
        .filter(|candidate| ignored.contains(*candidate))
        .count()
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn a7_ignore_vec_vs_hashset() {
    const REPS: usize = 200;
    for ignored_size in [100_usize, 400] {
        for candidate_count in [60_usize, 240] {
            let candidates: Vec<String> = (0..candidate_count)
                .map(|index| format!("v1.{index}.0"))
                .collect();
            let matched: Vec<String> = candidates.iter().step_by(4).cloned().collect();
            // Interleave pads between matched entries so the Vec scan sees a
            // realistic average position distribution.
            let mut ignored_vec: Vec<String> = Vec::with_capacity(ignored_size);
            let mut next_pad = 0_usize;
            for entry in matched {
                if ignored_vec.len() + 1 < ignored_size {
                    ignored_vec.push(format!("v9.{next_pad}.9"));
                    next_pad += 1;
                }
                ignored_vec.push(entry);
            }
            while ignored_vec.len() < ignored_size {
                ignored_vec.push(format!("v9.{next_pad}.9"));
                next_pad += 1;
            }
            let ignored_set: HashSet<String> = ignored_vec.iter().cloned().collect();

            let vec_matches = count_vec_matches(&ignored_vec, &candidates);
            let set_matches = count_set_matches(&ignored_set, &candidates);
            assert_eq!(
                vec_matches, set_matches,
                "A7 match counts differ at ignored={ignored_size} candidates={candidate_count}"
            );

            let mut vec_times = Vec::with_capacity(REPS);
            let mut set_times = Vec::with_capacity(REPS);
            for _ in 0..REPS {
                let started = Instant::now();
                let matches = count_vec_matches(&ignored_vec, &candidates);
                vec_times.push(started.elapsed());
                std::hint::black_box(matches);
                let started = Instant::now();
                let matches = count_set_matches(&ignored_set, &candidates);
                set_times.push(started.elapsed());
                std::hint::black_box(matches);
            }
            let vec_us = median_us(vec_times);
            let set_us = median_us(set_times);
            emit(
                "A7",
                &format!(
                    "ignored={ignored_size} candidates={candidate_count} matches={vec_matches} reps={REPS} vec_us={vec_us:.2} hashset_us={set_us:.2} ratio={:.1}x",
                    vec_us / set_us.max(1e-9),
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// A8: managed_paths pairwise cross check vs sorted scan (audit rejection
// spot-check)
// ---------------------------------------------------------------------------

fn normalized_components(path: &str) -> Vec<String> {
    path.to_ascii_lowercase()
        .split('\\')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_component_prefix(shorter: &[String], longer: &[String]) -> bool {
    shorter.len() <= longer.len() && shorter[..] == longer[..shorter.len()]
}

/// O(M^2) replica of the managed_paths pairwise cross check, counting prefix
/// overlaps (owner/kind exemptions are identical in both variants and
/// therefore omitted).
fn count_overlaps_pairwise(paths: &[Vec<String>]) -> u64 {
    let mut overlaps = 0_u64;
    for (index, left) in paths.iter().enumerate() {
        for right in paths.iter().skip(index + 1) {
            if is_component_prefix(left, right) || is_component_prefix(right, left) {
                overlaps += 1;
            }
        }
    }
    overlaps
}

/// Pairwise-equivalent sorted variant: after component-wise sorting, every
/// overlap pair (shorter, longer) is counted by scanning the contiguous run
/// of extensions following the shorter path. Sorting places a path strictly
/// before all of its extensions, and any element sorting between a path and
/// one of its extensions must itself carry that path as a prefix, so the
/// extension run is contiguous and no pair is missed or double-counted.
fn count_overlaps_sorted(mut paths: Vec<Vec<String>>) -> u64 {
    paths.sort();
    let mut overlaps = 0_u64;
    for (index, path) in paths.iter().enumerate() {
        let depth = path.len();
        let mut follower = index + 1;
        while follower < paths.len()
            && paths[follower].len() >= depth
            && paths[follower][..depth] == path[..depth]
        {
            overlaps += 1;
            follower += 1;
        }
    }
    overlaps
}

fn generate_managed_paths(count: usize) -> Vec<String> {
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        if index % 20 == 19 {
            // Child of an earlier tool destination: a real overlap.
            paths.push(format!(r"C:\utu\tools\tool{:04}\bin", index / 2));
        } else {
            paths.push(format!(r"C:\utu\tools\tool{:04}", index));
        }
    }
    paths
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture"]
fn a8_managed_paths_pairwise_vs_sorted() {
    for count in [50_usize, 200, 1000] {
        let raw = generate_managed_paths(count);
        let paths: Vec<Vec<String>> = raw.iter().map(|path| normalized_components(path)).collect();
        let pairwise = count_overlaps_pairwise(&paths);
        let sorted = count_overlaps_sorted(paths.clone());
        assert_eq!(
            pairwise, sorted,
            "A8 sorted variant lost pairwise equivalence at M={count}"
        );

        let reps = match count {
            50 => 200,
            200 => 50,
            _ => 10,
        };
        let mut pairwise_times = Vec::with_capacity(reps);
        let mut sorted_times = Vec::with_capacity(reps);
        for _ in 0..reps {
            let started = Instant::now();
            let overlaps = count_overlaps_pairwise(&paths);
            pairwise_times.push(started.elapsed());
            std::hint::black_box(overlaps);
            let started = Instant::now();
            let overlaps = count_overlaps_sorted(paths.clone());
            sorted_times.push(started.elapsed());
            std::hint::black_box(overlaps);
        }
        let pairwise_ms = median_ms(pairwise_times);
        let sorted_ms = median_ms(sorted_times);
        emit(
            "A8",
            &format!(
                "paths={count} overlaps={pairwise} reps={reps} pairwise_ms={pairwise_ms:.3} sorted_ms={sorted_ms:.3} ratio={:.1}x pairwise_comparisons={}",
                pairwise_ms / sorted_ms.max(1e-9),
                count * (count - 1) / 2,
            ),
        );
    }
}
