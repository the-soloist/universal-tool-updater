mod report;
mod selection;
mod show;
mod update;

use std::collections::BTreeSet;

use anyhow::Result;

use crate::cli::{Cli, Command};
use crate::config;
use crate::self_update::{self, SelfUpdateOptions};

use report::list_tools;
use selection::validate_profiles;
use show::show_distribution;
use update::{UpdateOptions, update_tools};

pub fn run(mut cli: Cli) -> Result<()> {
    let command = cli.command.take().unwrap_or(Command::Update {
        tools: Vec::new(),
        force: false,
        create_missing: false,
        dry_run: false,
        no_progress: false,
        jobs: None,
    });
    match &command {
        Command::Migrate { input, output } => {
            return config::migrate::migrate_directory(input, output);
        }
        Command::SelfUpdate { check, force } => {
            return self_update::run(SelfUpdateOptions {
                check_only: *check,
                force: *force,
            });
        }
        #[cfg(windows)]
        Command::SelfReplace {
            target,
            candidate,
            version,
        } => return self_update::replace_helper(target, candidate, version),
        #[cfg(windows)]
        Command::SelfCleanup { work_dir } => return self_update::cleanup_helper(work_dir),
        _ => {}
    }

    let manifest_path = cli.manifest_path()?;
    let config = config::load(&manifest_path)?;
    let profiles = cli.profile;
    let verbose = cli.verbose;
    match command {
        Command::Check => {
            validate_profiles(&config, &profiles)?;
            let profile_count = config
                .tools
                .values()
                .map(|tool| tool.profile.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            println!(
                "configuration valid: YAML files={}, profiles={}, tools={}, toolkit root {}, staging {}",
                profile_count + 1,
                profile_count,
                config.tools.len(),
                config.paths.toolkit_root.display(),
                config.paths.staging.display()
            );
            Ok(())
        }
        Command::List { tree } => {
            if tree {
                show_distribution(&config, &profiles)
            } else {
                list_tools(&config, &profiles)
            }
        }
        Command::Update {
            tools,
            force,
            create_missing,
            dry_run,
            no_progress,
            jobs,
        } => update_tools(
            &config,
            &tools,
            &profiles,
            UpdateOptions {
                force,
                create_missing,
                dry_run,
                no_progress,
                verbose,
                jobs: jobs.map(std::num::NonZeroUsize::get),
            },
        ),
        Command::Migrate { .. } | Command::SelfUpdate { .. } => {
            unreachable!("handled before loading configuration")
        }
        #[cfg(windows)]
        Command::SelfReplace { .. } | Command::SelfCleanup { .. } => {
            unreachable!("handled before loading configuration")
        }
    }
}
