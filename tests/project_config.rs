use std::path::Path;

use universal_tool_updater::config;
use universal_tool_updater::config::model::ReleaseConfig;

#[test]
fn all_project_manifests_are_valid_and_use_the_requested_toolkit_root() {
    let config = config::load(Path::new("profiles/manifest.yaml")).unwrap();
    let updater_directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    assert_eq!(config.tools.len(), 62);
    let mut github = 0;
    let mut web = 0;
    let mut http = 0;
    assert!(config.paths.toolkit_root.ends_with("Tools/Toolkit"));
    assert_eq!(config.paths.downloads, updater_directory.join("updates"));
    assert!(config.paths.state.starts_with(&config.paths.toolkit_root));
    for tool in config.tools.values() {
        match &tool.release {
            ReleaseConfig::Github { .. } => github += 1,
            ReleaseConfig::Web { .. } => web += 1,
            ReleaseConfig::Http { .. } => http += 1,
        }
        assert!(!tool.artifacts.is_empty(), "{} has no artifacts", tool.id);
        assert!(
            tool.install
                .destination
                .starts_with(&config.paths.toolkit_root),
            "{} escapes the toolkit root",
            tool.id
        );
    }
    assert_eq!((github, web, http), (41, 20, 1));
}
