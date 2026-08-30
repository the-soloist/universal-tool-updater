pub mod app;
pub mod archive;
pub mod cli;
pub mod config;
pub(crate) mod display;
pub(crate) mod domain;
pub(crate) mod downloader;
pub(crate) mod error;
pub(crate) mod hooks;
pub(crate) mod installer;
// The binary's logging layer shares the progress-aware writer, so this stays
// reachable as `universal_tool_updater::output`.
pub mod output;
pub(crate) mod paths;
pub(crate) mod progress;
pub(crate) mod resolver;
pub(crate) mod self_update;
pub(crate) mod state;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod workspace;
