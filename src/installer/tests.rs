use std::fs;

use tempfile::tempdir;

use crate::archive::ArchiveService;
use crate::config::model::{ArtifactConfig, ExistingPolicy, InputMode, OutputMode, ReleaseConfig};
#[cfg(unix)]
use crate::config::model::{HookAction, HookConfig};
use crate::domain::DownloadedArtifact;
#[cfg(unix)]
use crate::domain::SymlinkSpec;
use crate::hooks::HookRunner;
use crate::progress::ProgressManager;
use crate::test_support::tool as test_tool;
use crate::workspace::RunWorkspace;

use super::output::{effective_mode, managed_archive_path, managed_archive_pattern};
use super::transaction::CommitSource;
use super::{ExistingArchiveStatus, InstallOptions, Installer};

#[test]
fn cross_filesystem_fallback_copies_the_commit_source_next_to_the_destination() {
    let directory = tempdir().unwrap();
    let staging = directory.path().join("staging");
    let ready = staging.join("ready");
    let parent = directory.path().join("destination-parent");
    fs::create_dir_all(ready.join("bin")).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(ready.join("bin/demo"), "payload").unwrap();
    let version = staging.join(".version");
    fs::write(&version, "v2\n").unwrap();
    let mut tool = test_tool("demo", parent.join("Demo"));
    tool.name = "Demo".to_owned();
    tool.install.input = InputMode::Copy;

    let source =
        CommitSource::copy_next_to_destination(&tool, &ready, Some(&version), &parent).unwrap();
    let adjacent = source
        ._adjacent_transaction
        .as_ref()
        .unwrap()
        .path()
        .to_path_buf();
    assert!(source.ready.starts_with(&parent));
    assert_eq!(
        fs::read_to_string(source.ready.join("bin/demo")).unwrap(),
        "payload"
    );
    assert_eq!(
        fs::read_to_string(source.external_version.as_ref().unwrap()).unwrap(),
        "v2\n"
    );
    assert_eq!(
        fs::read_to_string(ready.join("bin/demo")).unwrap(),
        "payload"
    );
    drop(source);
    assert!(!adjacent.exists());
}

#[test]
fn keeps_github_copy_artifacts_uncompressed_and_records_release_version() {
    let directory = tempdir().unwrap();
    let toolkit = directory.path().join("Toolkit");
    let downloads = toolkit.join("updates");
    let destination = toolkit.join("Demo/release");
    fs::create_dir_all(&downloads).unwrap();

    let old_content = directory.path().join("old-content");
    fs::create_dir(&old_content).unwrap();
    fs::write(old_content.join("old.bin"), "old").unwrap();
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.parent().unwrap().join(".version"), "v1\n").unwrap();

    let archive_service = ArchiveService;
    archive_service
        .compress_7z(&old_content, &destination.join("Demo-v1.7z"))
        .unwrap();

    let asset = downloads.join("demo.zip");
    let assets = downloads.join("demo.xz");
    let source = downloads.join("demo-source.tar.gz");
    fs::write(&asset, "asset").unwrap();
    fs::write(&assets, "assets").unwrap();
    fs::write(&source, "source").unwrap();

    let mut tool = test_tool("demo", destination.clone());
    tool.name = "Demo".to_owned();
    tool.artifacts = vec![
        ArtifactConfig::GithubAsset {
            pattern: "demo.zip".to_owned(),
        },
        ArtifactConfig::GithubAssets {
            pattern: "demo.xz".to_owned(),
        },
        ArtifactConfig::GithubSource {
            format: "tar.gz".to_owned(),
        },
    ];
    tool.install.input = InputMode::Copy;
    tool.install.existing = ExistingPolicy::Merge;
    tool.install.save = OutputMode::Archive;
    assert_eq!(effective_mode(&tool), OutputMode::Directory);
    let mut extracted_tool = tool.clone();
    extracted_tool.install.input = InputMode::Extract;
    assert_eq!(effective_mode(&extracted_tool), OutputMode::Archive);
    let mut mixed_tool = tool.clone();
    mixed_tool.artifacts.push(ArtifactConfig::DirectUrl {
        url: "https://example.com/demo.bin".to_owned(),
    });
    assert_eq!(effective_mode(&mixed_tool), OutputMode::Directory);
    let hook_runner = HookRunner;
    let installer = Installer::new(&archive_service, &hook_runner, directory.path(), &toolkit);
    let run = RunWorkspace::create(&downloads, &downloads.join("staging")).unwrap();
    let workspace = run.prepare(&tool).unwrap();
    let progress = ProgressManager::new(false, 1);
    let task_progress = progress.task(&tool.profile, &tool.name);

    installer
        .install(
            &tool,
            "v2",
            &[
                DownloadedArtifact { path: asset },
                DownloadedArtifact { path: assets },
                DownloadedArtifact { path: source },
            ],
            &workspace,
            &task_progress,
            InstallOptions::new(1, ExistingArchiveStatus::Unchecked),
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(destination.join("old.bin")).unwrap(),
        "old"
    );
    assert_eq!(
        fs::read_to_string(destination.join("demo.zip")).unwrap(),
        "asset"
    );
    assert_eq!(
        fs::read_to_string(destination.join("demo.xz")).unwrap(),
        "assets"
    );
    assert_eq!(
        fs::read_to_string(destination.join("demo-source.tar.gz")).unwrap(),
        "source"
    );
    assert_eq!(
        fs::read_to_string(destination.parent().unwrap().join(".version")).unwrap(),
        "v2\n"
    );
    assert!(!destination.join(".version").exists());
    assert!(!destination.join("Demo-v1.7z").exists());
    assert!(!destination.join("Demo-v2.7z").exists());
    assert!(
        !destination
            .parent()
            .unwrap()
            .join(".version.utu-backup")
            .exists()
    );
}

#[test]
fn rebuilds_an_unchecked_corrupt_merge_archive() {
    let directory = tempdir().unwrap();
    let toolkit = directory.path().join("Toolkit");
    let downloads = directory.path().join("updates");
    let destination = toolkit.join("Demo");
    fs::create_dir_all(&destination).unwrap();
    fs::create_dir_all(&downloads).unwrap();
    fs::write(destination.join("Demo#v1.7z"), "corrupt archive").unwrap();

    let payload = directory.path().join("payload");
    let artifact = downloads.join("demo.7z");
    fs::create_dir(&payload).unwrap();
    fs::write(payload.join("new.bin"), "new").unwrap();
    let archive_service = ArchiveService;
    archive_service.compress_7z(&payload, &artifact).unwrap();

    let mut tool = test_tool("demo", destination.clone());
    tool.name = "Demo".to_owned();
    tool.install.existing = ExistingPolicy::Merge;
    tool.install.save = OutputMode::Archive;
    tool.install.archive_name = "{name}#{version}.7z".to_owned();
    let hook_runner = HookRunner;
    let installer = Installer::new(&archive_service, &hook_runner, directory.path(), &toolkit);
    let run = RunWorkspace::create(&downloads, &downloads.join("staging")).unwrap();
    let workspace = run.prepare(&tool).unwrap();
    let progress = ProgressManager::new(false, 1);
    let task_progress = progress.task(&tool.profile, &tool.name);

    installer
        .install(
            &tool,
            "v2",
            &[DownloadedArtifact { path: artifact }],
            &workspace,
            &task_progress,
            InstallOptions::new(1, ExistingArchiveStatus::Unchecked),
        )
        .unwrap();

    let archive = destination.join("Demo#v2.7z");
    let extracted = directory.path().join("extracted");
    assert!(!destination.join("Demo#v1.7z").exists());
    archive_service.extract(&archive, &extracted, None).unwrap();
    assert_eq!(
        fs::read_to_string(extracted.join("new.bin")).unwrap(),
        "new"
    );
}

#[test]
fn directory_output_records_version_inside_destination() {
    let directory = tempdir().unwrap();
    let toolkit = directory.path().join("Toolkit");
    let downloads = toolkit.join("updates");
    let destination = toolkit.join("Demo");
    fs::create_dir_all(&downloads).unwrap();

    let payload = directory.path().join("payload");
    fs::create_dir(&payload).unwrap();
    fs::write(payload.join("demo.bin"), "demo").unwrap();
    let artifact = downloads.join("demo.7z");
    let archive_service = ArchiveService;
    archive_service.compress_7z(&payload, &artifact).unwrap();

    let mut tool = test_tool("demo", destination.clone());
    tool.name = "Demo".to_owned();
    tool.release = ReleaseConfig::Web {
        url: "https://example.com/demo".to_owned(),
        version_pattern: "Version (.+)".to_owned(),
        ignore_versions: Vec::new(),
    };
    tool.artifacts = vec![ArtifactConfig::DirectUrl {
        url: "https://example.com/demo.7z".to_owned(),
    }];
    let hook_runner = HookRunner;
    let installer = Installer::new(&archive_service, &hook_runner, directory.path(), &toolkit);
    let run = RunWorkspace::create(&downloads, &downloads.join("staging")).unwrap();
    let workspace = run.prepare(&tool).unwrap();
    let progress = ProgressManager::new(false, 1);
    let task_progress = progress.task(&tool.profile, &tool.name);

    installer
        .install(
            &tool,
            "v2.1.0",
            &[DownloadedArtifact { path: artifact }],
            &workspace,
            &task_progress,
            InstallOptions::new(1, ExistingArchiveStatus::Unchecked),
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(destination.join("demo.bin")).unwrap(),
        "demo"
    );
    assert_eq!(
        fs::read_to_string(destination.join(".version")).unwrap(),
        "v2.1.0\n"
    );
    assert!(!toolkit.join(".version").exists());
}

#[cfg(unix)]
#[test]
fn restores_previous_installation_when_post_install_hook_fails() {
    let directory = tempdir().unwrap();
    let toolkit = directory.path().join("Toolkit");
    let downloads = toolkit.join("updates");
    let tool_root = toolkit.join("Demo");
    let destination = tool_root.join("release");
    fs::create_dir_all(&downloads).unwrap();
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("old.txt"), "old").unwrap();
    fs::write(tool_root.join(".version"), "v1\n").unwrap();
    let link_target = directory.path().join("bin/demo");
    fs::create_dir_all(link_target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(destination.join("old.txt"), &link_target).unwrap();
    let artifact = downloads.join("new.bin");
    fs::write(&artifact, "new").unwrap();
    let mut tool = test_tool("demo", destination.clone());
    tool.name = "Demo".to_owned();
    tool.artifacts = vec![ArtifactConfig::GithubAsset {
        pattern: "demo".to_owned(),
    }];
    tool.install.input = InputMode::Copy;
    tool.install.symlinks = vec![SymlinkSpec {
        from: "new.bin".into(),
        to: link_target.clone(),
    }];
    tool.hooks = HookConfig {
        after_install: vec![HookAction::Rename {
            from: "new.bin".to_owned(),
            to: "renamed.bin".into(),
        }],
        ..HookConfig::default()
    };
    let archive_service = ArchiveService;
    let hook_runner = HookRunner;
    let installer = Installer::new(&archive_service, &hook_runner, directory.path(), &toolkit);
    let run = RunWorkspace::create(&downloads, &downloads.join("staging")).unwrap();
    let workspace = run.prepare(&tool).unwrap();
    let progress = ProgressManager::new(false, 1);
    let task_progress = progress.task(&tool.profile, &tool.name);

    let result = installer.install(
        &tool,
        "v2",
        &[DownloadedArtifact { path: artifact }],
        &workspace,
        &task_progress,
        InstallOptions::new(1, ExistingArchiveStatus::Unchecked),
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(destination.join("old.txt")).unwrap(),
        "old"
    );
    assert!(!destination.join("new.bin").exists());
    assert_eq!(
        fs::read_to_string(tool_root.join(".version")).unwrap(),
        "v1\n"
    );
    assert_eq!(
        fs::read_link(&link_target).unwrap(),
        destination.join("old.txt")
    );
    assert!(!tool_root.join(".version.utu-backup").exists());
}

#[test]
fn recognizes_only_updater_managed_archives_for_merging() {
    let directory = tempdir().unwrap();
    let destination = directory.path().join("Demo");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("third-party.7z"), "not an updater archive").unwrap();
    let mut tool = test_tool("demo", destination.clone());
    tool.name = "Demo".to_owned();
    tool.install.existing = ExistingPolicy::Merge;
    tool.install.save = OutputMode::Archive;
    tool.install.archive_name = "{name}#{version}.7z".to_owned();

    assert!(managed_archive_path(&tool, &destination).unwrap().is_none());
    fs::write(destination.join("Demo#1.2.3.7z"), "managed").unwrap();
    assert_eq!(
        managed_archive_path(&tool, &destination).unwrap(),
        Some(destination.join("Demo#1.2.3.7z"))
    );
    let pattern = managed_archive_pattern(&tool);
    assert!(pattern.is_match("Demo#1.2.3.7z"));
    assert!(!pattern.is_match("third-party.7z"));
}
