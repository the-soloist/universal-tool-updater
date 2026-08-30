use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use tempfile::tempdir;
use universal_tool_updater::app;
use universal_tool_updater::archive::ArchiveService;
use universal_tool_updater::cli::{Cli, Command};
use universal_tool_updater::config::model::{
    ArtifactConfig, DefaultsConfig, ExistingPolicy, ExtractionLimits, HookAction, HookConfig,
    InstallConfig, ManifestFile, NetworkConfig, OutputMode, PathConfig, ReleaseConfig,
    SCHEMA_VERSION, ToolConfig, ToolFile,
};
use zip::write::SimpleFileOptions;

#[test]
fn resolves_downloads_and_repairs_a_corrupt_merge_archive() {
    let workspace = tempdir().unwrap();
    let toolkit = workspace.path().join("Toolkit");
    let config_dir = workspace.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let archive = zip_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap();
            let (content_type, body): (&str, Vec<u8>) = match path {
                "/release" => ("text/html", b"<h1>Version 1.2.3</h1>".to_vec()),
                "/demo.zip" => ("application/zip", archive.clone()),
                other => panic!("unexpected request {other}"),
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });

    let mut tools = BTreeMap::new();
    tools.insert(
        "demo".to_owned(),
        ToolConfig {
            name: Some("Demo".to_owned()),
            enabled: true,
            release: ReleaseConfig::Web {
                url: format!("http://{address}/release"),
                version_pattern: r"Version (\d+\.\d+\.\d+)".to_owned(),
                ignore_versions: Vec::new(),
            },
            artifacts: vec![ArtifactConfig::DirectUrl {
                url: format!("http://{address}/demo.zip"),
                sha256: None,
            }],
            install: InstallConfig {
                destination: PathBuf::from("Demo"),
                ..InstallConfig::default()
            },
            hooks: HookConfig {
                after_unpack: vec![HookAction::Rename {
                    from: "tool.txt".to_owned(),
                    to: PathBuf::from("updater.txt"),
                }],
                ..HookConfig::default()
            },
        },
    );
    fs::write(
        config_dir.join("tools.yaml"),
        yaml_serde::to_string(&ToolFile { tools }).unwrap(),
    )
    .unwrap();
    let mut defaults = DefaultsConfig::default();
    defaults.install.existing = ExistingPolicy::Merge;
    defaults.install.save = OutputMode::Archive;
    defaults.install.archive_name = "{id}-{version}.7z".to_owned();
    let manifest = ManifestFile {
        schema_version: SCHEMA_VERSION,
        include: vec!["tools.yaml".to_owned()],
        paths: PathConfig {
            toolkit_root: toolkit.clone(),
            downloads: workspace.path().join("updates"),
            staging: None,
            state: PathBuf::from(".updater/state.yaml"),
        },
        network: NetworkConfig {
            progress: false,
            ..NetworkConfig::default()
        },
        defaults,
        extraction_limits: ExtractionLimits::default(),
        allow_insecure_transports: true,
    };
    let manifest_path = config_dir.join("manifest.yaml");
    fs::write(&manifest_path, yaml_serde::to_string(&manifest).unwrap()).unwrap();

    app::run(Cli {
        manifest: Some(manifest_path.clone()),
        profiles: None,
        verbose: false,
        log_dir: None,
        profile: Vec::new(),
        command: Some(Command::Update {
            tools: vec!["demo".to_owned()],
            force: false,
            create_missing: false,
            dry_run: false,
            no_progress: true,
            jobs: None,
        }),
    })
    .unwrap();

    let saved_archive = toolkit.join("Demo/demo-1.2.3.7z");
    assert!(saved_archive.is_file());
    fs::write(&saved_archive, "corrupt archive").unwrap();

    // 同版本归档损坏时，merge 不能再次读取坏包，应直接从可信发布产物重建。
    app::run(Cli {
        manifest: Some(manifest_path),
        profiles: None,
        verbose: false,
        log_dir: None,
        profile: Vec::new(),
        command: Some(Command::Update {
            tools: vec!["demo".to_owned()],
            force: false,
            create_missing: false,
            dry_run: false,
            no_progress: true,
            jobs: None,
        }),
    })
    .unwrap();
    server.join().unwrap();

    let extracted = workspace.path().join("saved-archive");
    ArchiveService::default()
        .extract(&saved_archive, &extracted, None)
        .unwrap();
    assert_eq!(
        fs::read_to_string(extracted.join("bin/updater.txt")).unwrap(),
        "installed"
    );
    let state = fs::read_to_string(toolkit.join(".updater/state.yaml")).unwrap();
    assert!(state.contains("version: 1.2.3"));
}

#[test]
fn runs_complete_tool_updates_in_parallel() {
    let workspace = tempdir().unwrap();
    let toolkit = workspace.path().join("Toolkit");
    let config_dir = workspace.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let archive = Arc::new(zip_fixture());
    let active_downloads = Arc::new(AtomicUsize::new(0));
    let maximum_downloads = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn({
        let active_downloads = Arc::clone(&active_downloads);
        let maximum_downloads = Arc::clone(&maximum_downloads);
        move || {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let archive = Arc::clone(&archive);
                let active_downloads = Arc::clone(&active_downloads);
                let maximum_downloads = Arc::clone(&maximum_downloads);
                requests.push(thread::spawn(move || {
                    let mut request = [0_u8; 2048];
                    let size = stream.read(&mut request).unwrap();
                    let request = String::from_utf8_lossy(&request[..size]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap();
                    let body = if path.starts_with("/release-") {
                        b"<h1>Version 1.2.3</h1>".to_vec()
                    } else if path.ends_with(".zip") {
                        let active = active_downloads.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum_downloads.fetch_max(active, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(150));
                        active_downloads.fetch_sub(1, Ordering::SeqCst);
                        archive.as_ref().clone()
                    } else {
                        panic!("unexpected request {path}");
                    };
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(&body).unwrap();
                }));
            }
            for request in requests {
                request.join().unwrap();
            }
        }
    });

    let mut tools = BTreeMap::new();
    for id in ["alpha", "beta"] {
        tools.insert(
            id.to_owned(),
            ToolConfig {
                name: Some(id.to_uppercase()),
                enabled: true,
                release: ReleaseConfig::Web {
                    url: format!("http://{address}/release-{id}"),
                    version_pattern: r"Version (\d+\.\d+\.\d+)".to_owned(),
                    ignore_versions: Vec::new(),
                },
                artifacts: vec![ArtifactConfig::DirectUrl {
                    url: format!("http://{address}/{id}.zip"),
                    sha256: None,
                }],
                install: InstallConfig {
                    destination: PathBuf::from(id),
                    ..InstallConfig::default()
                },
                hooks: HookConfig::default(),
            },
        );
    }
    fs::write(
        config_dir.join("tools.yaml"),
        yaml_serde::to_string(&ToolFile { tools }).unwrap(),
    )
    .unwrap();
    let mut defaults = DefaultsConfig::default();
    defaults.install.save = OutputMode::Archive;
    defaults.install.archive_name = "{id}-{version}.7z".to_owned();
    let manifest = ManifestFile {
        schema_version: SCHEMA_VERSION,
        include: vec!["tools.yaml".to_owned()],
        paths: PathConfig {
            toolkit_root: toolkit.clone(),
            downloads: workspace.path().join("updates"),
            staging: None,
            state: PathBuf::from(".updater/state.yaml"),
        },
        network: NetworkConfig {
            progress: false,
            jobs: 2,
            ..NetworkConfig::default()
        },
        defaults,
        extraction_limits: ExtractionLimits::default(),
        allow_insecure_transports: true,
    };
    let manifest_path = config_dir.join("manifest.yaml");
    fs::write(&manifest_path, yaml_serde::to_string(&manifest).unwrap()).unwrap();

    app::run(Cli {
        manifest: Some(manifest_path),
        profiles: None,
        verbose: false,
        log_dir: None,
        profile: Vec::new(),
        command: Some(Command::Update {
            tools: Vec::new(),
            force: false,
            create_missing: false,
            dry_run: false,
            no_progress: true,
            jobs: None,
        }),
    })
    .unwrap();
    server.join().unwrap();

    assert_eq!(maximum_downloads.load(Ordering::SeqCst), 2);
    assert!(toolkit.join("alpha/alpha-1.2.3.7z").is_file());
    assert!(toolkit.join("beta/beta-1.2.3.7z").is_file());
    let state = fs::read_to_string(toolkit.join(".updater/state.yaml")).unwrap();
    assert!(state.contains("alpha:"));
    assert!(state.contains("beta:"));
    let staging = workspace.path().join("updates/staging");
    let update_entries = fs::read_dir(workspace.path().join("updates"))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(update_entries.len(), 1);
    assert_eq!(update_entries[0].path(), staging);
    assert!(fs::read_dir(&staging).unwrap().next().is_none());
}

#[test]
fn skips_manual_placeholders_instead_of_updating() {
    let workspace = tempdir().unwrap();
    let toolkit = workspace.path().join("Toolkit");
    let config_dir = workspace.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let mut tools = BTreeMap::new();
    tools.insert(
        "ida-pro".to_owned(),
        ToolConfig {
            name: Some("IDA Pro".to_owned()),
            enabled: true,
            release: ReleaseConfig::Manual {},
            artifacts: Vec::new(),
            install: InstallConfig {
                destination: PathBuf::from("Reverse/Decompiler/IDA Pro"),
                ..InstallConfig::default()
            },
            hooks: HookConfig::default(),
        },
    );
    fs::write(
        config_dir.join("tools.yaml"),
        yaml_serde::to_string(&ToolFile { tools }).unwrap(),
    )
    .unwrap();
    let manifest = ManifestFile {
        schema_version: SCHEMA_VERSION,
        include: vec!["tools.yaml".to_owned()],
        paths: PathConfig {
            toolkit_root: toolkit.clone(),
            downloads: workspace.path().join("updates"),
            staging: None,
            state: PathBuf::from(".updater/state.yaml"),
        },
        network: NetworkConfig {
            progress: false,
            ..NetworkConfig::default()
        },
        defaults: DefaultsConfig::default(),
        extraction_limits: ExtractionLimits::default(),
        allow_insecure_transports: false,
    };
    let manifest_path = config_dir.join("manifest.yaml");
    fs::write(&manifest_path, yaml_serde::to_string(&manifest).unwrap()).unwrap();

    // No HTTP server is started: a manual tool must be skipped before resolution.
    app::run(Cli {
        manifest: Some(manifest_path),
        profiles: None,
        verbose: false,
        log_dir: None,
        profile: Vec::new(),
        command: Some(Command::Update {
            tools: vec!["ida-pro".to_owned()],
            force: false,
            create_missing: false,
            dry_run: false,
            no_progress: true,
            jobs: None,
        }),
    })
    .unwrap();

    assert!(!toolkit.join("Reverse").exists());
    assert!(!toolkit.join(".updater/state.yaml").exists());
}

fn zip_fixture() -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    archive
        .start_file("demo-1.2.3/bin/tool.txt", SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"installed").unwrap();
    archive.finish().unwrap().into_inner()
}
