use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use tempfile::{TempDir, tempdir};

use crate::config::model::{ArtifactConfig, InputMode, ReleaseConfig};
use crate::domain::{DownloadedArtifact, ResolvedArtifact, Tool};
use crate::progress::ProgressManager;
use crate::test_support::tool as test_tool;
use crate::workspace::{RunWorkspace, ToolWorkspace};

use super::http::is_retryable_status;
use super::partial::{
    Metadata as PartialMetadata, SCHEMA_VERSION as PARTIAL_SCHEMA_VERSION, paths as partial_paths,
    save as save_partial_metadata,
};
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
                url: url.to_owned(),
                filename: "artifact.bin".to_owned(),
                etag: Some(etag.to_owned()),
                last_modified: None,
                total,
            },
        )
        .unwrap();
    }

    fn download(&self, url: &str) -> DownloadedArtifact {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let progress = ProgressManager::new(false, 1);
        let task_progress = progress.task("test", "Resume");
        Downloader::new(client)
            .download(
                &self.tool,
                &ResolvedArtifact {
                    url: url.to_owned(),
                    filename: Some("artifact.bin".to_owned()),
                },
                &self.workspace,
                0,
                1,
                &task_progress,
            )
            .unwrap()
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
