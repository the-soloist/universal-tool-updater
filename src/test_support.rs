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
        release: ReleaseConfig::Github {
            repository: format!("owner/{id}"),
            ignore_versions: Vec::new(),
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
            executable: Vec::new(),
            symlinks: Vec::new(),
        },
        hooks: HookConfig::default(),
    }
}
