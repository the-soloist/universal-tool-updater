use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "updater", version, about)]
pub struct Cli {
    /// Path to a manifest file.
    #[arg(long, global = true)]
    pub manifest: Option<PathBuf>,

    /// Profile directory containing manifest.yaml and its included YAML files.
    #[arg(long, global = true, value_name = "DIR", conflicts_with = "manifest")]
    pub profiles: Option<PathBuf>,

    /// Enable diagnostic logs.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Directory used to retain a separate log for every run.
    #[arg(long, global = true, value_name = "DIR")]
    pub log_dir: Option<PathBuf>,

    /// Only process tools from these included profiles.
    #[arg(
        short = 'p',
        long = "profile",
        visible_alias = "group",
        visible_short_alias = 'g',
        global = true
    )]
    pub profile: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Resolve, download and install configured tools.
    Update {
        /// Tool IDs. Omit to update every selected tool.
        tools: Vec<String>,

        /// Ignore the recorded version.
        #[arg(short, long)]
        force: bool,

        /// Create destinations even when a tool overrides the default.
        #[arg(long)]
        create_missing: bool,

        /// Resolve and print the plan without downloading or installing.
        #[arg(long)]
        dry_run: bool,

        /// Hide the update progress display.
        #[arg(long)]
        no_progress: bool,

        /// Override network.jobs from the manifest.
        #[arg(long)]
        jobs: Option<NonZeroUsize>,
    },

    /// List configured tools.
    List {
        /// Display tools as a profile and directory hierarchy table.
        #[arg(long)]
        tree: bool,
    },

    /// Validate every included YAML value and cross-field constraint without network access.
    Check,

    /// Convert legacy TOML files to the current YAML schema.
    Migrate {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

impl Cli {
    pub fn manifest_path(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = &self.manifest {
            return Ok(path.clone());
        }
        let profiles = self
            .profiles
            .clone()
            .unwrap_or_else(default_profiles_directory);
        Ok(profiles.join("manifest.yaml"))
    }
}

fn default_profiles_directory() -> PathBuf {
    PathBuf::from("profiles")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn reads_manifest_from_the_selected_profiles_directory() {
        let cli =
            Cli::try_parse_from(["updater", "--profiles", "custom-profiles", "check"]).unwrap();
        assert_eq!(
            cli.manifest_path().unwrap(),
            Path::new("custom-profiles/manifest.yaml")
        );
        assert!(
            Cli::try_parse_from([
                "updater",
                "--manifest",
                "manifest.yaml",
                "--profiles",
                "profiles",
                "check",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["updater", "--platform", "windows", "check"]).is_err());
    }

    #[test]
    fn uses_the_flat_profiles_directory_by_default() {
        let cli = Cli::try_parse_from(["updater", "check"]).unwrap();
        assert_eq!(
            cli.manifest_path().unwrap(),
            Path::new("profiles/manifest.yaml")
        );
    }

    #[test]
    fn parses_tree_as_a_list_option_and_profile_as_a_global_option() {
        let list = Cli::try_parse_from([
            "updater",
            "--profiles",
            "profiles",
            "list",
            "--tree",
            "--profile",
            "tools",
        ])
        .unwrap();
        assert_eq!(list.profile, ["tools"]);
        assert!(matches!(list.command, Some(Command::List { tree: true })));

        assert!(Cli::try_parse_from(["updater", "--show"]).is_err());

        let update = Cli::try_parse_from(["updater", "update", "--group", "tools"]).unwrap();
        assert_eq!(update.profile, ["tools"]);
        assert!(matches!(update.command, Some(Command::Update { .. })));
    }

    #[test]
    fn tree_is_no_longer_a_subcommand() {
        assert!(Cli::try_parse_from(["updater", "tree"]).is_err());
    }

    #[test]
    fn parses_positive_parallel_job_limit() {
        let cli = Cli::try_parse_from(["updater", "update", "--jobs", "3"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                jobs: Some(value),
                ..
            }) if value.get() == 3
        ));
        assert!(Cli::try_parse_from(["updater", "update", "--jobs", "0"]).is_err());
    }
}
