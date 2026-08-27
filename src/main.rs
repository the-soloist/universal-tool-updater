use std::process::ExitCode;

use clap::Parser;

use universal_tool_updater::{app, cli::Cli};

mod logging;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let log_path = match logging::init(cli.verbose, cli.log_dir.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("ERROR cannot initialize persistent logging: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let arguments = std::env::args_os()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!(log = %log_path.display(), command = %arguments, "updater run started");

    match app::run(cli) {
        Ok(()) => {
            tracing::info!("updater run completed");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
