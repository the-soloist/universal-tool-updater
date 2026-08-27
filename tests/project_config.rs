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
