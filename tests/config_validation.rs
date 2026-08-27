use std::fs;

use tempfile::tempdir;
use universal_tool_updater::config;
use universal_tool_updater::config::model::{ManifestFile, OutputMode};

#[test]
fn reads_parallel_jobs_from_the_manifest() {
    let manifest: ManifestFile = yaml_serde::from_str(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
network:
  jobs: 7
"#,
    )
    .unwrap();

    assert_eq!(manifest.network.jobs, 7);
}

#[test]
fn rejects_zero_parallel_jobs_in_the_manifest() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
network:
  jobs: 0
"#,
    )
    .unwrap();

    let error = config::load(&directory.path().join("manifest.yaml")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("network.jobs must be greater than zero")
    );
}

#[test]
fn rejects_invalid_manifest_parameter_values() {
    for (manifest, expected) in [
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ''
"#,
            "paths.toolkit_root must not be empty",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
  staging: ''
"#,
            "paths.staging must not be empty",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
network:
  user_agent: ''
"#,
            "network.user_agent must not be empty",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
network:
  timeout_seconds: 0
"#,
            "network.timeout_seconds must be greater than zero",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
network:
  github_token_env: GITHUB-TOKEN
"#,
            "network.github_token_env must be a portable environment variable name",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
defaults:
  install:
    archive_name: '{unknown}.7z'
"#,
            "unsupported placeholder {unknown}",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
defaults:
  install:
    save: archive
    archive_name: tool.zip
"#,
            "must use the .7z extension",
        ),
    ] {
        assert_invalid_manifest(manifest, expected);
    }
}

#[test]
fn uses_save_for_global_output_mode_and_accepts_the_legacy_alias() {
    let manifest: ManifestFile = yaml_serde::from_str(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
defaults:
  install:
    output: archive
"#,
    )
    .unwrap();

    assert_eq!(manifest.defaults.install.save, OutputMode::Archive);
    let encoded = yaml_serde::to_string(&manifest).unwrap();
    assert!(encoded.contains("save: archive"));
    assert!(!encoded.contains("output:"));
}

#[test]
fn derives_the_release_directory_from_copy_input() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("tools.yaml"),
        r#"
tools:
  automatic:
    release:
      type: github
      repository: owner/automatic
    artifacts:
      - type: github-source
        format: tar.gz
    install:
      destination: Reverse/automatic
      input: copy
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert!(
        loaded.tools["automatic"]
            .install
            .destination
            .ends_with("Reverse/automatic/release")
    );
}

#[test]
fn derives_profiles_only_from_manifest_includes() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 5
include: [web.yaml]
paths:
  toolkit_root: Toolkit
"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("web.yaml"),
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Web/Demo
"#,
    )
    .unwrap();
    fs::write(directory.path().join("ignored.yaml"), "not: [valid").unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert_eq!(loaded.tools.len(), 1);
    assert_eq!(loaded.tools["demo"].profile, "web");
    assert_eq!(loaded.paths.staging, loaded.paths.downloads.join("staging"));
}

#[test]
fn resolves_an_explicit_staging_directory_from_the_updater_directory() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
  downloads: custom-downloads
  staging: custom-staging
"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("tools.yaml"),
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    let updater_directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(
        loaded.paths.downloads,
        updater_directory.join("custom-downloads")
    );
    assert_eq!(
        loaded.paths.staging,
        updater_directory.join("custom-staging")
    );
}

#[test]
fn rejects_a_manually_appended_release_directory_for_copy_input() {
    assert_invalid_tool(
        r#"
tools:
  legacy:
    release:
      type: github
      repository: owner/legacy
    artifacts:
      - type: github-source
        format: tar.gz
    install:
      destination: Reverse/legacy/release
      input: copy
"#,
        "must not end with 'release'",
    );
}

#[test]
fn rejects_unknown_fields_and_parent_path_traversal() {
    assert_invalid_tool(
        r#"
tools:
  demo:
    name: Demo
    mystery: true
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "unknown field",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    name: Demo
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: ../escape
"#,
        "may not contain '..'",
    );
}

#[test]
fn rejects_tool_ids_that_are_not_kebab_case() {
    assert_invalid_tool(
        r#"
tools:
  ContextMenuManager:
    name: ContextMenuManager
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "lowercase kebab-case",
    );
}

#[test]
fn rejects_invalid_release_and_artifact_values() {
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: 'owner /demo'
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "GitHub repository must be a valid owner/name",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: web
      url: file:///tmp/releases.html
      version_pattern: 'version=(.+)'
    artifacts:
      - type: page-link
        pattern: 'href="([^"]+)"'
    install:
      destination: Demo
"#,
        "must use HTTP or HTTPS",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: http
      url: https://example.com/demo.zip
      version_headers: [etag, ETag]
    artifacts:
      - type: release-url
    install:
      destination: Demo
"#,
        "duplicate HTTP version header",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
      ignore_versions: [v1, v1]
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "duplicate ignore_versions entry",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: url-template
        url: https://example.com/{version}/{platform}.zip
    install:
      destination: Demo
"#,
        "unsupported placeholder {platform}",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: direct-url
        url: https://example.com/demo.zip
      - type: direct-url
        url: https://example.com/demo.zip
    install:
      destination: Demo
"#,
        "duplicate artifact configuration",
    );
}

#[test]
fn rejects_conflicting_install_parameters() {
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-source
        format: zip
    install:
      destination: Demo
      input: copy
      archive_password: secret
"#,
        "archive_password conflicts with input copy",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: web
      url: https://example.com/releases
      version_pattern: 'version=(.+)'
    artifacts:
      - type: direct-url
        url: https://example.com/demo.zip
    install:
      destination: Demo
      save: archive
      symlinks:
        - from: demo
          to: bin/demo
"#,
        "symlinks require directory output",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
      executable: [demo, demo]
"#,
        "duplicate executable path",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
      symlinks:
        - from: demo.exe
          to: Demo/demo.exe
"#,
        "conflicts with its source",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    name: '   '
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "name must not be empty",
    );
}

#[test]
fn rejects_empty_profiles_and_runtime_path_conflicts() {
    assert_invalid_tool("tools: {}\n", "tools must not be empty");
    assert_invalid_tool_with_manifest(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
  downloads: ~/Tools/Toolkit/Demo/cache
  state: .updater/state.yaml
"#,
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "conflicts with paths.downloads",
    );
    assert_invalid_tool_with_manifest(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
  downloads: updates
  staging: ~/Tools/Toolkit/Demo/transactions
  state: .updater/state.yaml
"#,
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
        "conflicts with paths.staging",
    );
}

#[test]
fn rejects_non_python_external_hooks_and_native_actions_in_the_wrong_stage() {
    assert_invalid_tool(
        r#"
tools:
  demo:
    name: Demo
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
    hooks:
      after_unpack:
        - type: python
          script: scripts/demo.bat
"#,
        "must use the .py extension",
    );
    assert_invalid_tool(
        r#"
tools:
  demo:
    name: Demo
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
    hooks:
      after_install:
        - type: rename
          from: demo-*.exe
          to: demo.exe
"#,
        "rename is only valid after_unpack",
    );
}

#[test]
fn rejects_shared_symlink_targets_before_parallel_updates_start() {
    assert_invalid_tool(
        r#"
tools:
  alpha:
    release:
      type: github
      repository: owner/alpha
    artifacts:
      - type: github-asset
        pattern: alpha
    install:
      destination: Alpha
      symlinks:
        - from: alpha
          to: bin/shared
  beta:
    release:
      type: github
      repository: owner/beta
    artifacts:
      - type: github-asset
        pattern: beta
    install:
      destination: Beta
      symlinks:
        - from: beta
          to: bin/shared
"#,
        "share symlink target",
    );
}

#[test]
fn rejects_nested_installation_destinations_before_parallel_updates_start() {
    assert_invalid_tool(
        r#"
tools:
  alpha:
    release:
      type: github
      repository: owner/alpha
    artifacts:
      - type: github-asset
        pattern: alpha
    install:
      destination: Shared
  beta:
    release:
      type: github
      repository: owner/beta
    artifacts:
      - type: github-asset
        pattern: beta
    install:
      destination: Shared/Beta
"#,
        "overlapping destinations",
    );
}

fn assert_invalid_tool(tool_file: &str, expected: &str) {
    assert_invalid_tool_with_manifest(
        r#"
schema_version: 5
include:
  - tools.yaml
paths:
  toolkit_root: ~/Tools/Toolkit
  downloads: updates
  state: .updater/test-state.yaml
"#,
        tool_file,
        expected,
    );
}

fn assert_invalid_manifest(manifest: &str, expected: &str) {
    let directory = tempdir().unwrap();
    fs::write(directory.path().join("manifest.yaml"), manifest).unwrap();
    let error = config::load(&directory.path().join("manifest.yaml")).unwrap_err();
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?}, got {error:#}"
    );
}

fn assert_invalid_tool_with_manifest(manifest: &str, tool_file: &str, expected: &str) {
    let directory = tempdir().unwrap();
    fs::write(directory.path().join("manifest.yaml"), manifest).unwrap();
    fs::write(directory.path().join("tools.yaml"), tool_file).unwrap();
    let error = config::load(&directory.path().join("manifest.yaml")).unwrap_err();
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?}, got {error:#}"
    );
}
