use std::fs;

use tempfile::tempdir;
use universal_tool_updater::archive::ExtractionLimits;
use universal_tool_updater::config;
use universal_tool_updater::config::model::{ManifestFile, OutputMode, ReleaseConfig};

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
fn rejects_state_nested_inside_the_staging_directory() {
    let directory = tempdir().unwrap();
    let toolkit = yaml_path(directory.path());
    let staging = yaml_path(&directory.path().join("transactions"));
    let state = yaml_path(&directory.path().join("transactions/state.yaml"));
    let manifest = format!(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: '{toolkit}'
  staging: '{staging}'
  state: '{state}'
"#
    );

    assert_invalid_manifest(&manifest, "paths.staging");
}

#[test]
fn uses_save_for_global_output_mode_and_rejects_the_legacy_alias() {
    let manifest: ManifestFile = yaml_serde::from_str(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
defaults:
  install:
    save: archive
"#,
    )
    .unwrap();

    assert_eq!(manifest.defaults.install.save, OutputMode::Archive);
    let encoded = yaml_serde::to_string(&manifest).unwrap();
    assert!(encoded.contains("save: archive"));
    assert!(!encoded.contains("output:"));

    let legacy = r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
defaults:
  install:
    output: archive
"#;
    assert!(yaml_serde::from_str::<ManifestFile>(legacy).is_err());
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
fn reads_the_github_prerelease_opt_in_without_changing_schema_version() {
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
  stable:
    release:
      type: github
      repository: owner/stable
    artifacts:
      - type: github-asset
        pattern: stable.zip
    install:
      destination: Stable
  cutting-edge:
    release:
      type: github
      repository: owner/cutting-edge
      allow_prereleases: true
    artifacts:
      - type: github-asset
        pattern: edge.zip
    install:
      destination: CuttingEdge
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert!(matches!(
        loaded.tools["stable"].release,
        ReleaseConfig::Github {
            allow_prereleases: false,
            ..
        }
    ));
    assert!(matches!(
        loaded.tools["cutting-edge"].release,
        ReleaseConfig::Github {
            allow_prereleases: true,
            ..
        }
    ));
}

#[test]
fn reads_extraction_limits_overrides_without_changing_schema_version() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
extraction_limits:
  max_total_bytes: 4096
  max_entries: 5
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
      repository: owner/repo
    artifacts:
      - type: github-asset
        pattern: demo.zip
    install:
      destination: Demo
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert_eq!(loaded.extraction_limits.max_total_bytes, 4096);
    assert_eq!(loaded.extraction_limits.max_entries, 5);

    // 整个节点省略时回落到默认配额。
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
    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert_eq!(loaded.extraction_limits, ExtractionLimits::default());
}

#[test]
fn rejects_non_positive_extraction_limits() {
    for (manifest, expected) in [
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
extraction_limits:
  max_total_bytes: 0
"#,
            "extraction_limits.max_total_bytes must be greater than zero",
        ),
        (
            r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
extraction_limits:
  max_entries: 0
"#,
            "extraction_limits.max_entries must be greater than zero",
        ),
    ] {
        assert_invalid_manifest(manifest, expected);
    }
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
    assert_invalid_tool_with_manifest(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: ~/Tools/Toolkit
  state: .updater/../Demo/.version
"#,
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo.exe
    install:
      destination: Demo
      input: copy
"#,
        "version marker",
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
      destination: Beta
      symlinks:
        - from: beta
          to: shared/beta
"#,
        "conflicts with tool alpha destination",
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
      destination: shared/Beta
"#,
        "overlapping destinations",
    );
}

#[test]
fn rejects_destinations_that_collide_with_transaction_backups() {
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
      destination: Shared.utu-backup
"#,
        "transaction backup",
    );
}

#[test]
fn rejects_reserved_paths_that_collide_with_transaction_backups() {
    assert_invalid_tool_with_manifest(
        r#"
schema_version: 5
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
  downloads: updates
  state: Demo.utu-backup
"#,
        r#"
tools:
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: demo
    install:
      destination: Demo
"#,
        "transaction backup",
    );
}

#[test]
fn rejects_destinations_that_conflict_with_external_version_markers() {
    assert_invalid_tool(
        r#"
tools:
  copied:
    release:
      type: github
      repository: owner/copied
    artifacts:
      - type: github-asset
        pattern: copied.exe
    install:
      destination: Shared
      input: copy
  marker-owner:
    release:
      type: github
      repository: owner/marker-owner
    artifacts:
      - type: github-asset
        pattern: marker-owner.zip
    install:
      destination: Shared/.version
"#,
        "conflicts with tool copied version marker",
    );
}

#[test]
fn accepts_manual_release_without_artifacts() {
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
  ida-pro:
    name: IDA Pro
    release:
      type: manual
    install:
      destination: Reverse/Decompiler/IDA Pro
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();

    assert!(matches!(
        &loaded.tools["ida-pro"].release,
        ReleaseConfig::Manual {}
    ));
    assert!(loaded.tools["ida-pro"].artifacts.is_empty());
}

#[test]
fn rejects_artifacts_for_manual_release() {
    assert_invalid_tool(
        r#"
tools:
  ida-pro:
    release:
      type: manual
    artifacts:
      - type: direct-url
        url: https://example.com/ida-pro.zip
    install:
      destination: Reverse/Decompiler/IDA Pro
"#,
        "manual tools are maintained manually and must not configure artifacts",
    );
}

#[test]
fn rejects_unknown_manual_release_fields() {
    assert_invalid_tool(
        r#"
tools:
  ida-pro:
    release:
      type: manual
      repository: owner/ida-pro
    install:
      destination: Reverse/Decompiler/IDA Pro
"#,
        "unknown field",
    );
}

#[test]
fn parses_the_install_symlink_opt_in_and_defaults_to_false() {
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
  plain:
    release:
      type: github
      repository: owner/plain
    artifacts:
      - type: github-asset
        pattern: plain.tar.gz
    install:
      destination: Plain
  linked:
    release:
      type: github
      repository: owner/linked
    artifacts:
      - type: github-asset
        pattern: linked.tar.gz
    install:
      destination: Linked
      allow_symlinks_in_archive: true
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert!(!loaded.tools["plain"].install.allow_symlinks_in_archive);
    assert!(loaded.tools["linked"].install.allow_symlinks_in_archive);
}

#[test]
fn rejects_non_boolean_allow_symlinks_in_archive() {
    assert_invalid_tool(
        r#"
tools:
  linked:
    release:
      type: github
      repository: owner/linked
    artifacts:
      - type: github-asset
        pattern: linked.tar.gz
    install:
      destination: Linked
      allow_symlinks_in_archive: banana
"#,
        "invalid type",
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

fn yaml_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\'', "''")
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
