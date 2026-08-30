use std::fs;
use std::path::Path;

use tempfile::tempdir;
use universal_tool_updater::config;
use universal_tool_updater::config::model::ReleaseConfig;

#[test]
fn local_profile_manifest_is_valid_when_present() {
    let manifest = Path::new("profiles/manifest.yaml");
    if !manifest.is_file() {
        return;
    }

    let config = config::load(manifest).unwrap();
    let updater_directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    assert!(!config.tools.is_empty());
    assert!(config.paths.toolkit_root.ends_with("Tools/Toolkit"));
    assert_eq!(config.paths.downloads, updater_directory.join("updates"));
    assert_eq!(config.paths.staging, config.paths.downloads.join("staging"));
    assert!(config.paths.state.starts_with(&config.paths.toolkit_root));
    for tool in config.tools.values() {
        assert!(!tool.artifacts.is_empty(), "{} has no artifacts", tool.id);
        assert!(
            tool.install
                .destination
                .starts_with(&config.paths.toolkit_root),
            "{} escapes the toolkit root",
            tool.id
        );
    }
}

#[test]
fn example_profile_remains_valid_and_never_enables_downloadable_tools() {
    let source = Path::new("examples/profile.yaml");
    assert!(source.is_file(), "example profile is missing");

    let directory = tempdir().unwrap();
    fs::copy(source, directory.path().join("example.yaml")).unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 6
include: [example.yaml]
paths:
  toolkit_root: ExampleToolkit
  downloads: example-updates
  state: .updater/example-state.yaml
defaults:
  install:
    input: extract
    existing: replace
    save: directory
    strip_single_root: true
    archive_name: '{name}#{version}.7z'
"#,
    )
    .unwrap();

    let loaded = config::load(&directory.path().join("manifest.yaml")).unwrap();
    assert_eq!(loaded.tools.len(), 6);
    // Manual placeholders never download, so they may stay enabled; everything
    // else in the example profile must remain disabled.
    assert!(
        loaded
            .tools
            .values()
            .all(|tool| { !tool.enabled || matches!(tool.release, ReleaseConfig::Manual {}) })
    );
}
