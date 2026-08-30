use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub(super) fn expected_sha256(contents: &str, filename: &str) -> Result<String> {
    let mut matches = contents.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let listed = fields.next()?.trim_start_matches('*');
        (listed == filename).then_some(digest)
    });
    let digest = matches
        .next()
        .with_context(|| format!("SHA256SUMS.txt does not contain {filename:?}"))?;
    if matches.next().is_some() {
        bail!("SHA256SUMS.txt contains duplicate entries for {filename:?}");
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("SHA256SUMS.txt contains an invalid SHA-256 digest for {filename:?}");
    }
    Ok(digest.to_ascii_lowercase())
}

pub(super) fn verify(path: &Path, expected: &str) -> Result<()> {
    let file = File::open(path)
        .with_context(|| format!("cannot open self-update archive {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("cannot hash self-update archive {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut actual = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut actual, "{byte:02x}").expect("writing to a String cannot fail");
    }
    if actual != expected {
        bail!(
            "self-update archive checksum mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{expected_sha256, verify};

    #[test]
    fn selects_an_exact_checksum_entry() {
        let sums = concat!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  updater-v1.0.0-linux-arm64.7z\n",
            "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB *updater-v1.0.0-windows-arm64.7z\n",
        );
        assert_eq!(
            expected_sha256(sums, "updater-v1.0.0-windows-arm64.7z").unwrap(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert!(expected_sha256(sums, "updater-v1.0.0-macos-arm64.7z").is_err());
    }

    #[test]
    fn verifies_file_contents_and_rejects_corruption() {
        let directory = tempdir().unwrap();
        let archive = directory.path().join("updater.7z");
        fs::write(&archive, b"release archive").unwrap();
        let mut expected = String::with_capacity(64);
        for byte in Sha256::digest(b"release archive") {
            write!(&mut expected, "{byte:02x}").unwrap();
        }
        verify(&archive, &expected).unwrap();
        assert!(verify(&archive, &"0".repeat(64)).is_err());
    }
}
