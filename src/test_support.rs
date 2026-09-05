use std::path::PathBuf;

use crate::domain::{
    ExistingPolicy, HookConfig, InputMode, InstallSpec, OutputMode, ReleaseConfig, Tool,
};

pub(crate) fn tool(id: &str, destination: impl Into<PathBuf>) -> Tool {
    Tool {
        id: id.to_owned(),
        name: id.to_owned(),
        profile: "test".to_owned(),
        enabled: true,
        allow_insecure_transports: false,
        release: ReleaseConfig::Github {
            repository: format!("owner/{id}"),
            ignore_versions: Vec::new(),
            allow_prereleases: false,
        },
        artifacts: Vec::new(),
        install: InstallSpec {
            destination: destination.into(),
            input: InputMode::Extract,
            existing: ExistingPolicy::Replace,
            save: OutputMode::Directory,
            strip_single_root: true,
            create_destination: true,
            archive_name: "{name}-{version}.7z".to_owned(),
            archive_password: None,
            allow_symlinks_in_archive: false,
            executable: Vec::new(),
            symlinks: Vec::new(),
        },
        hooks: HookConfig::default(),
    }
}
