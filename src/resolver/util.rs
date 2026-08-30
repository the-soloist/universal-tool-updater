use std::io::Read;

use crate::domain::{ArtifactConfig, Tool};
use crate::error::UpdaterError;
use anyhow::Result;
use chardetng::EncodingDetector;
use reqwest::blocking::{Client, Response};

/// Response bodies beyond this size are rejected before decoding so a
/// hostile release page cannot exhaust memory through the text resolvers.
pub(super) const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn get_text(client: &Client, tool: &Tool, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("request to {url} failed: {error}"),
        })?;
    let bytes = read_body_limited(response, tool, url)?;
    Ok(decode(&bytes))
}

/// Reads at most MAX_RESPONSE_BODY_BYTES: the declared Content-Length is
/// checked up front, and the stream itself is capped so chunked or lying
/// responses hit the same ceiling.
pub(super) fn read_body_limited(
    response: Response,
    tool: &Tool,
    url: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    if let Some(length) = response.content_length()
        && length > MAX_RESPONSE_BODY_BYTES as u64
    {
        return Err(body_too_large(tool, url, length));
    }
    // One extra byte so an over-limit body is detected, not silently cut.
    let mut limited = response.take((MAX_RESPONSE_BODY_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| UpdaterError::Resolution {
            tool: tool.id.clone(),
            message: format!("cannot read response from {url}: {error}"),
        })?;
    if bytes.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(body_too_large(tool, url, bytes.len() as u64));
    }
    Ok(bytes)
}

fn body_too_large(tool: &Tool, url: &str, length: u64) -> anyhow::Error {
    UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!(
            "response from {url} is {length} bytes, exceeding the {MAX_RESPONSE_BODY_BYTES} byte limit"
        ),
    }
    .into()
}

/// Extracts the configured SHA-256 digest for checksum-capable artifact types,
/// rendering `{version}` templates against the resolved release version.
pub(super) fn expected_sha256(artifact: &ArtifactConfig, version: &str) -> Option<String> {
    let sha256 = match artifact {
        ArtifactConfig::DirectUrl { sha256, .. }
        | ArtifactConfig::UrlTemplate { sha256, .. }
        | ArtifactConfig::GithubAsset { sha256, .. }
        | ArtifactConfig::GithubAssets { sha256, .. }
        | ArtifactConfig::GithubSource { sha256, .. } => sha256,
        _ => return None,
    };
    sha256
        .as_ref()
        .map(|value| value.replace("{version}", version))
}

pub(super) fn decode(bytes: &[u8]) -> String {
    let mut detector = EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, true);
    let (decoded, _, _) = encoding.decode(bytes);
    decoded.into_owned()
}

pub(super) fn incompatible_artifact(tool: &Tool, artifact: &str) -> UpdaterError {
    UpdaterError::Resolution {
        tool: tool.id.clone(),
        message: format!("artifact type {artifact} is incompatible with this release provider"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;

    use reqwest::blocking::Client;

    use crate::domain::ArtifactConfig;
    use crate::test_support::tool as test_tool;

    use super::{MAX_RESPONSE_BODY_BYTES, expected_sha256, get_text};

    fn demo_tool() -> crate::domain::Tool {
        test_tool("demo", PathBuf::from("/toolkit/demo"))
    }

    /// Accepts one request, drains it, and lets the closure write the reply.
    fn serve_once(reply: impl FnOnce(&mut std::net::TcpStream) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/page", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).unwrap();
            let _ = &request[..size];
            reply(&mut stream);
        });
        url
    }

    #[test]
    fn reads_bodies_within_the_limit_and_keeps_them_decodable() {
        let url = serve_once(|stream| {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nv1.23"
            )
            .unwrap();
        });

        let text = get_text(&Client::new(), &demo_tool(), &url).unwrap();
        assert_eq!(text, "v1.23");
    }

    #[test]
    fn rejects_bodies_whose_declared_length_exceeds_the_limit() {
        let url = serve_once(|stream| {
            // The declared length alone must abort the read; the body is
            // never drained.
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RESPONSE_BODY_BYTES as u64 + 1
            )
            .unwrap();
        });

        let error = get_text(&Client::new(), &demo_tool(), &url).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeding the 16777216 byte limit"),
            "expected the size-limit rejection, got {error:#}"
        );
        assert!(error.to_string().contains("/page"));
    }

    #[test]
    fn caps_streamed_bodies_without_a_declared_length() {
        let url = serve_once(|stream| {
            // No Content-Length: the body ends at connection close, so only
            // the stream-side cap can stop the read.
            write!(stream, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n").unwrap();
            let chunk = [b'a'; 64 * 1024];
            // 16 MiB + one extra chunk; write errors after the client drops
            // the connection are expected and ignored.
            for _ in 0..(MAX_RESPONSE_BODY_BYTES / chunk.len() + 1) {
                if stream.write_all(&chunk).is_err() {
                    break;
                }
            }
        });

        let error = get_text(&Client::new(), &demo_tool(), &url).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeding the 16777216 byte limit"),
            "expected the streaming cap, got {error:#}"
        );
    }

    #[test]
    fn read_body_limited_reports_the_url_and_size() {
        // Exercised through the public error text so the URL and byte count
        // stay part of the contract even without a live response.
        let error = super::body_too_large(&demo_tool(), "https://example.com/x", 17 * 1024 * 1024);
        assert!(error.to_string().contains("https://example.com/x"));
        assert!(error.to_string().contains("17825792"));
    }

    #[test]
    fn passes_direct_urls_through_and_renders_version_templates() {
        let fixed = ArtifactConfig::DirectUrl {
            url: "https://example.com/tool.zip".to_owned(),
            sha256: Some("a".repeat(64)),
        };
        let template = ArtifactConfig::UrlTemplate {
            url: "https://example.com/{version}/tool.zip".to_owned(),
            sha256: Some(format!("b{{version}}{}", "c".repeat(54))),
        };
        assert_eq!(expected_sha256(&fixed, "v2.0.0"), Some("a".repeat(64)));
        assert_eq!(
            expected_sha256(&template, "v2.0.0"),
            Some(format!("bv2.0.0{}", "c".repeat(54)))
        );
        assert_eq!(expected_sha256(&ArtifactConfig::ReleaseUrl, "v2.0.0"), None);
    }

    #[test]
    fn applies_github_pins_and_renders_their_version_templates() {
        let asset = ArtifactConfig::GithubAsset {
            pattern: r"^tool\.zip$".to_owned(),
            sha256: Some(format!("d{{version}}{}", "e".repeat(54))),
        };
        let assets = ArtifactConfig::GithubAssets {
            pattern: r"^tool-.+\.zip$".to_owned(),
            sha256: Some("f".repeat(64)),
        };
        let source = ArtifactConfig::GithubSource {
            format: "tar.gz".to_owned(),
            sha256: Some("f".repeat(64)),
        };
        assert_eq!(
            expected_sha256(&asset, "v3.0.0"),
            Some(format!("dv3.0.0{}", "e".repeat(54)))
        );
        assert_eq!(expected_sha256(&assets, "v3.0.0"), Some("f".repeat(64)));
        assert_eq!(expected_sha256(&source, "v3.0.0"), Some("f".repeat(64)));
        assert_eq!(
            expected_sha256(
                &ArtifactConfig::GithubAsset {
                    pattern: r"^tool\.zip$".to_owned(),
                    sha256: None,
                },
                "v3.0.0"
            ),
            None
        );
    }
}
