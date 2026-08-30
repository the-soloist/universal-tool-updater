use std::fs;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn check_command_validates_all_included_yaml_files() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 6
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
  demo:
    release:
      type: github
      repository: owner/demo
    artifacts:
      - type: github-asset
        pattern: '^demo-.+\.zip$'
    install:
      destination: Demo
"#,
    )
    .unwrap();

    let output = updater(directory.path()).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("configuration valid: YAML files=2, profiles=1, tools=1"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn check_command_returns_failure_for_an_invalid_value() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("manifest.yaml"),
        r#"
schema_version: 6
include: [tools.yaml]
paths:
  toolkit_root: Toolkit
network:
  timeout_seconds: 0
"#,
    )
    .unwrap();

    let output = updater(directory.path()).output().unwrap();
    assert!(!output.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("network.timeout_seconds must be greater than zero"),
        "{diagnostic}"
    );
}

fn updater(profiles: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_updater"));
    command
        .arg("--profiles")
        .arg(profiles)
        .arg("--log-dir")
        .arg(profiles.join("logs"))
        .arg("check");
    command
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
