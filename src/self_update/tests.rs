use std::fs;

use tempfile::tempdir;

use super::asset_platform;
use super::candidate::find_candidate;
use super::lock::{LOCK_FILENAME, UpdateLock};

#[test]
fn maps_only_published_platforms() {
    assert_eq!(asset_platform("linux", "x86_64").unwrap(), "linux-x86_64");
    assert_eq!(asset_platform("linux", "aarch64").unwrap(), "linux-arm64");
    assert_eq!(
        asset_platform("windows", "x86_64").unwrap(),
        "windows-x86_64"
    );
    assert_eq!(
        asset_platform("windows", "aarch64").unwrap(),
        "windows-arm64"
    );
    assert_eq!(asset_platform("macos", "aarch64").unwrap(), "macos-arm64");
    assert_eq!(asset_platform("macos", "x86_64").unwrap(), "macos-x86_64");
    assert!(asset_platform("linux", "armv7").is_err());
}

#[test]
fn requires_one_exact_candidate_file() {
    let directory = tempdir().unwrap();
    let expected = if cfg!(windows) {
        "updater.exe"
    } else {
        "updater"
    };
    let candidate = directory.path().join(expected);
    fs::write(&candidate, "binary").unwrap();
    assert_eq!(find_candidate(directory.path()).unwrap(), candidate);
    fs::write(directory.path().join("unexpected"), "extra").unwrap();
    assert!(find_candidate(directory.path()).is_err());
}

#[test]
fn prevents_concurrent_self_updates() {
    let directory = tempdir().unwrap();
    let first = UpdateLock::acquire(directory.path()).unwrap();
    assert!(UpdateLock::acquire(directory.path()).is_err());
    drop(first);
    assert!(directory.path().join(LOCK_FILENAME).exists());
    UpdateLock::acquire(directory.path()).unwrap();
}

#[test]
fn hands_the_process_lock_to_the_replacement_helper() {
    let directory = tempdir().unwrap();
    let first = UpdateLock::acquire(directory.path()).unwrap();
    first.release_for_handoff().unwrap();
    let helper = UpdateLock::acquire_for_helper(directory.path()).unwrap();
    assert_eq!(helper.path, directory.path().join(LOCK_FILENAME));
}
