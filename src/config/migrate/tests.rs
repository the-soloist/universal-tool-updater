use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;

use crate::config;

use super::convert::{normalize_destination, normalize_symlink_target, slug};
use super::migrate_directory;

#[test]
fn normalizes_legacy_destinations_under_toolkit_root() {
    assert_eq!(
        normalize_destination("../../Web/Scanner"),
        Path::new("Web/Scanner")
    );
    assert_eq!(
        normalize_destination("/opt/tools/bat"),
        Path::new("Tools/bat")
    );
    assert_eq!(
        normalize_symlink_target("/opt/binary/bat"),
        Path::new("bin/bat")
    );
}

#[test]
fn creates_stable_ascii_ids() {
    assert_eq!(slug("ShiroEXP - safe6Sec"), "shiro-exp-safe6-sec");
    assert_eq!(slug("ContextMenuManager"), "context-menu-manager");
    assert_eq!(slug("UniGetUI"), "uni-get-ui");
}

#[test]
fn migrates_a_legacy_file_to_loadable_yaml_schema() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("linux");
    let output = directory.path().join("v5");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("tools.toml"),
        r#"
[UpdaterConfig]
disable_repack = true

[bat]
folder = "/opt/tools/bat"
url = "sharkdp/bat"
local_version = "v0.1.0"
from = "github"
re_download = ["bat-linux.tar.gz"]
unpack = false
is_release = true
"#,
    )
    .unwrap();

    migrate_directory(&input, &output).unwrap();
    let loaded = config::load(&output.join("manifest.yaml")).unwrap();
    assert_eq!(loaded.tools.len(), 1);
    assert!(
        loaded.tools["bat"]
            .install
            .destination
            .ends_with("Tools/Toolkit/Tools/bat/release")
    );
    let migrated: crate::config::model::ToolFile =
        yaml_serde::from_str(&fs::read_to_string(output.join("tools.yaml")).unwrap()).unwrap();
    assert_eq!(
        migrated.tools["bat"].install.destination,
        PathBuf::from("Tools").join("bat")
    );
}

#[test]
fn migrates_plain_http_sources_with_an_explicit_insecure_opt_in() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("linux");
    let output = directory.path().join("v5");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("tools.toml"),
        r#"
[demo]
folder = "/opt/tools/demo"
url = "http://example.com/releases"
update_url = "http://example.com/demo.zip"
re_version = 'Version (.+)'
from = "web"
"#,
    )
    .unwrap();

    migrate_directory(&input, &output).unwrap();

    let loaded = config::load(&output.join("manifest.yaml")).unwrap();
    assert_eq!(loaded.tools.len(), 1);
    assert!(loaded.tools["demo"].allow_insecure_transports);
    let profile = fs::read_to_string(output.join("tools.yaml")).unwrap();
    assert!(profile.contains("allow_insecure_transports: true"));
}

#[test]
fn migrates_https_sources_without_the_insecure_opt_in() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("linux");
    let output = directory.path().join("v5");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("tools.toml"),
        r#"
[demo]
folder = "/opt/tools/demo"
url = "https://example.com/releases"
update_url = "https://example.com/demo.zip"
re_version = 'Version (.+)'
from = "web"
"#,
    )
    .unwrap();

    migrate_directory(&input, &output).unwrap();

    let loaded = config::load(&output.join("manifest.yaml")).unwrap();
    assert_eq!(loaded.tools.len(), 1);
    assert!(!loaded.tools["demo"].allow_insecure_transports);
    let manifest = fs::read_to_string(output.join("manifest.yaml")).unwrap();
    assert!(!manifest.contains("allow_insecure_transports"));
}

#[test]
fn rejects_non_python_legacy_hooks() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("linux");
    let output = directory.path().join("v5");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("tools.toml"),
        r#"
[demo]
folder = "/opt/tools/demo"
url = "owner/demo"
from = "github"
re_download = ["demo.zip"]
post_unpack = "./scripts/demo.bat"
"#,
    )
    .unwrap();

    let error = migrate_directory(&input, &output).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("only permits Python external scripts")
    );
}

#[test]
fn rejects_legacy_files_without_tools() {
    let directory = tempdir().unwrap();
    let input = directory.path().join("linux");
    let output = directory.path().join("v5");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("empty.toml"),
        "[UpdaterConfig]\ndisable_repack = true\n",
    )
    .unwrap();

    let error = migrate_directory(&input, &output).unwrap_err();

    assert!(error.to_string().contains("contains no tools"));
}
