use std::path::Path;

use universal_tool_updater::config;

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
    assert!(
        config.paths.toolkit_root.is_absolute(),
        "toolkit root should be resolved to an absolute path"
    );
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
fn example_manifest_and_profile_remain_valid_and_disabled() {
    let manifest = Path::new("examples/manifest.yaml");
    assert!(manifest.is_file(), "example manifest is missing");

    let loaded = config::load(manifest).unwrap();
    assert_eq!(loaded.tools.len(), 4);
    assert_eq!(loaded.network.jobs, 4);
    assert!(loaded.tools.values().all(|tool| !tool.enabled));
}
