use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};

use crate::config::model::{ArtifactConfig, InputMode, ReleaseConfig};
use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::progress::ProgressManager;
use crate::test_support::tool as test_tool;
use crate::workspace::{RunWorkspace, ToolWorkspace};

use super::partial::{
    Metadata as PartialMetadata, SCHEMA_VERSION as PARTIAL_SCHEMA_VERSION, hash_prefix,
    paths as partial_paths, save as save_partial_metadata,
};
use super::transfer::is_retryable_status;
use super::{DownloadCompletion, Downloader};

#[test]
fn resumes_a_partial_download_from_a_previous_run() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
        assert!(request.contains("range: bytes=6-"));
        assert!(request.contains("if-range: \"resume-v1\""));
        write!(
            stream,
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 6-10/11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(&body[6..]).unwrap();
    });

    let fixture = DownloadFixture::new();
    fixture.cache(&url, &body[..6], Some(body.len() as u64), "\"resume-v1\"");
    let downloaded = fixture.download(&url);
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

#[test]
fn resumes_after_the_response_body_is_interrupted() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            if request_number == 0 {
                assert!(!request.contains("range:"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.write_all(&body[..6]).unwrap();
            } else {
                assert!(request.contains("range: bytes=6-"));
                assert!(request.contains("if-range: \"resume-v1\""));
                write!(
                    stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 6-10/11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.write_all(&body[6..]).unwrap();
            }
        }
    });

    let fixture = DownloadFixture::new();
    let downloaded = fixture.download(&url);
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

#[test]
fn downloads_more_partial_content_chunks_than_the_retry_limit() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for (chunk, start) in [0_usize, 3, 6, 9].into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            if chunk == 0 {
                assert!(!request.contains("range:"));
            } else {
                assert!(request.contains(&format!("range: bytes={start}-")));
                assert!(request.contains("if-range: \"chunked-v1\""));
            }
            let end = (start + 2).min(body.len() - 1);
            let length = end - start + 1;
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: \"chunked-v1\"\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body[start..=end]).unwrap();
        }
    });

    let fixture = DownloadFixture::new();
    let downloaded = fixture.download(&url);
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

#[test]
fn revalidates_a_complete_partial_before_using_it() {
    let cached = b"hello old!";
    let current = b"hello new!";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
        assert!(request.contains("range: bytes=10-"));
        assert!(request.contains("if-range: \"resume-v1\""));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nETag: \"resume-v2\"\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(current).unwrap();
    });

    let fixture = DownloadFixture::new();
    fixture.cache(&url, cached, Some(cached.len() as u64), "\"resume-v1\"");
    let downloaded = fixture.download(&url);
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), current);
    fixture.assert_partials_empty();
}

#[test]
fn restarts_when_a_416_response_carries_no_validator() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            if request_number == 0 {
                assert!(request.contains("range: bytes=6-"));
                assert!(request.contains("if-range: \"resume-v1\""));
                // A 416 without any validator must not be trusted as "unchanged".
                write!(
                    stream,
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */11\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            } else {
                assert!(
                    !request.contains("range:"),
                    "the download must restart from zero"
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nETag: \"resume-v2\"\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        }
    });

    let fixture = DownloadFixture::new();
    fixture.cache(&url, &body[..6], Some(body.len() as u64), "\"resume-v1\"");
    let downloaded = fixture.download(&url);
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

#[test]
fn discards_a_tampered_partial_and_restarts_from_zero() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
        assert!(
            !request.contains("range:"),
            "a tampered partial must not be resumed"
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });

    let fixture = DownloadFixture::new();
    fixture.cache(&url, &body[..6], Some(body.len() as u64), "\"resume-v1\"");
    // Flip one byte of the cached prefix without changing its length.
    let (partial, _metadata) = partial_paths(fixture.workspace.partials(), &url);
    let mut contents = body[..6].to_vec();
    contents[2] ^= 0x01;
    fs::write(&partial, &contents).unwrap();
    let downloaded = fixture.download(&url);
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

#[test]
fn retries_transport_and_transient_http_failures() {
    assert!(is_retryable_status(None));
    assert!(is_retryable_status(Some(StatusCode::REQUEST_TIMEOUT)));
    assert!(is_retryable_status(Some(StatusCode::TOO_MANY_REQUESTS)));
    assert!(is_retryable_status(Some(StatusCode::BAD_GATEWAY)));
}

#[test]
fn does_not_retry_permanent_http_failures() {
    assert!(!is_retryable_status(Some(StatusCode::BAD_REQUEST)));
    assert!(!is_retryable_status(Some(StatusCode::NOT_FOUND)));
}

#[test]
fn rejects_duplicate_artifact_filenames_instead_of_overwriting() {
    let directory = tempdir().unwrap();
    let tool = resume_tool();
    let artifact = ResolvedArtifact {
        url: "https://example.com/second".to_owned(),
        filename: Some("artifact.bin".to_owned()),
        expected_sha256: None,
    };
    let partial = directory.path().join("partial");
    fs::write(directory.path().join("artifact.bin"), "first").unwrap();
    fs::write(&partial, "second").unwrap();
    let completion = DownloadCompletion {
        tool: &tool,
        artifact: &artifact,
        directory: directory.path(),
        index: 1,
        artifacts: 2,
    };

    let error = completion
        .finalize(
            &partial,
            &directory.path().join("metadata"),
            "artifact.bin",
            6,
            None,
        )
        .unwrap_err();

    assert!(error.to_string().contains("same filename"));
    assert_eq!(
        fs::read_to_string(directory.path().join("artifact.bin")).unwrap(),
        "first"
    );
    assert_eq!(fs::read_to_string(partial).unwrap(), "second");
}

fn resume_tool() -> Tool {
    let mut tool = test_tool("resume-test", "resume-test");
    tool.name = "Resume Test".to_owned();
    tool.release = ReleaseConfig::Http {
        url: "https://example.com/artifact.bin".to_owned(),
        version_headers: Vec::new(),
    };
    tool.artifacts = vec![ArtifactConfig::ReleaseUrl];
    tool.install.input = InputMode::Copy;
    tool
}

#[test]
fn rejects_chunked_responses_that_exceed_the_download_ceiling() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let _request = String::from_utf8_lossy(&request[..size]);
        // Chunked encoding carries no Content-Length, so only the
        // output-side byte counter can stop the stream.
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nETag: \"chunked-limit\"\r\nConnection: close\r\n\r\n");
        for _ in 0..16 {
            let _ = stream.write_all(b"20\r\n");
            let _ = stream.write_all(&[0_u8; 32]);
            let _ = stream.write_all(b"\r\n");
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });

    let fixture = DownloadFixture::new();
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let progress = ProgressManager::new(false, 1);
    let task_progress = progress.task("test", "Limit");
    let error = Downloader::new(
        client,
        crate::archive::Limits {
            max_total_bytes: 64,
            max_entries: 1,
        },
    )
    .download(
        &fixture.tool,
        &ResolvedArtifact {
            url: url.clone(),
            filename: Some("artifact.bin".to_owned()),
            expected_sha256: None,
        },
        &fixture.workspace,
        0,
        1,
        &task_progress,
    )
    .unwrap_err();
    server.join().unwrap();

    let message = format!("{error:#}");
    assert!(
        message.contains("exceeding the extraction limit of 64 bytes"),
        "expected the byte-ceiling failure, got {message}"
    );
    fixture.assert_partials_empty();
}

#[test]
fn fails_within_a_bounded_number_of_restarts_against_an_oscillating_server() {
    let body = [0_u8; 101];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let _server = thread::spawn(move || {
        // Every response ignores the Range request and restarts from byte 0,
        // never completing the declared 1000-byte total.
        for _ in 0..16 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let _request = String::from_utf8_lossy(&request[..size]);
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: 101\r\nContent-Range: bytes 0-100/1000\r\nETag: \"oscillate-v1\"\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let fixture = DownloadFixture::new();
        let result = fixture.download_with_sha256(&url, None);
        let _ = sender.send(result.map(|_| ()));
    });
    let error = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("the oscillating download must terminate instead of hanging")
        .unwrap_err();

    assert!(
        error.to_string().contains("restarted"),
        "expected a restart-limit failure, got {error:#}"
    );
}

#[test]
fn verifies_the_expected_sha256_digest_after_downloading() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let _request = String::from_utf8_lossy(&request[..size]);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });

    let fixture = DownloadFixture::new();
    let digest = hex_sha256(body);
    let downloaded = fixture.download_with_sha256(&url, Some(digest)).unwrap();
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

/// An interrupted body that retries and resumes must still verify against
/// the streaming digest accumulated across both rounds (A1 regression).
#[test]
fn verifies_the_sha256_digest_after_an_interrupted_transfer_resumes() {
    let body = pseudo_random_body(200_000);
    let interrupted = 65_636_usize;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server_body = body.clone();
    let server = thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            if request_number == 0 {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"interrupt-v1\"\r\nConnection: close\r\n\r\n",
                    server_body.len()
                )
                .unwrap();
                // Send less than the declared length, then drop the socket so
                // the body read fails and the transfer retries.
                stream.write_all(&server_body[..interrupted]).unwrap();
            } else {
                assert!(request.contains(&format!("range: bytes={interrupted}-")));
                write!(
                    stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"interrupt-v1\"\r\nConnection: close\r\n\r\n",
                    server_body.len() - interrupted,
                    interrupted,
                    server_body.len() - 1,
                    server_body.len()
                )
                .unwrap();
                stream.write_all(&server_body[interrupted..]).unwrap();
            }
        }
    });

    let fixture = DownloadFixture::new();
    let digest = hex_sha256(&body);
    let downloaded = fixture.download_with_sha256(&url, Some(digest)).unwrap();
    server.join().unwrap();

    assert_eq!(fs::read(downloaded.path).unwrap(), body);
    fixture.assert_partials_empty();
}

fn pseudo_random_body(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 32) as u8
        })
        .collect()
}

#[test]
fn rejects_a_mismatching_sha256_digest_and_clears_the_partial_cache() {
    let body = b"hello world";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/artifact.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let _request = String::from_utf8_lossy(&request[..size]);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });

    let fixture = DownloadFixture::new();
    let error = fixture
        .download_with_sha256(&url, Some("0".repeat(64)))
        .unwrap_err();
    server.join().unwrap();

    assert!(
        error.to_string().contains("sha256 checksum mismatch"),
        "expected a checksum failure, got {error:#}"
    );
    fixture.assert_partials_empty();
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct DownloadFixture {
    _root: TempDir,
    _run: RunWorkspace,
    tool: Tool,
    workspace: ToolWorkspace,
}

impl DownloadFixture {
    fn new() -> Self {
        let root = tempdir().unwrap();
        let tool = resume_tool();
        let run = RunWorkspace::create(root.path(), &root.path().join("staging")).unwrap();
        let workspace = run.prepare(&tool).unwrap();
        Self {
            _root: root,
            _run: run,
            tool,
            workspace,
        }
    }

    fn cache(&self, url: &str, contents: &[u8], total: Option<u64>, etag: &str) {
        let (partial, metadata) = partial_paths(self.workspace.partials(), url);
        fs::write(&partial, contents).unwrap();
        save_partial_metadata(
            &metadata,
            &PartialMetadata {
                schema_version: PARTIAL_SCHEMA_VERSION,
                filename: "artifact.bin".to_owned(),
                etag: Some(etag.to_owned()),
                last_modified: None,
                total,
                prefix_sha256: hash_prefix(&partial, contents.len() as u64).unwrap(),
                prefix_len: contents.len() as u64,
            },
        )
        .unwrap();
    }

    fn download(&self, url: &str) -> DownloadedArtifact {
        self.download_with_sha256(url, None).unwrap()
    }

    fn download_with_sha256(
        &self,
        url: &str,
        expected_sha256: Option<String>,
    ) -> Result<DownloadedArtifact, anyhow::Error> {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let progress = ProgressManager::new(false, 1);
        let task_progress = progress.task("test", "Resume");
        Downloader::new(client, crate::archive::Limits::default()).download(
            &self.tool,
            &ResolvedArtifact {
                url: url.to_owned(),
                filename: Some("artifact.bin".to_owned()),
                expected_sha256,
            },
            &self.workspace,
            0,
            1,
            &task_progress,
        )
    }

    fn assert_partials_empty(&self) {
        assert!(
            fs::read_dir(self.workspace.partials())
                .unwrap()
                .next()
                .is_none()
        );
    }
}
