mod installation;
mod links;
mod source;

use std::path::Path;

use anyhow::Result;

use crate::domain::{Tool, VERSION_FILE};
use crate::hooks::{HookContext, HookRunner, HookStage};

use installation::InstallationTransaction;
use links::LinkTransaction;
pub(super) use source::{CommitSource, same_filesystem};

pub(super) struct CommitRequest<'a> {
    pub(super) tool: &'a Tool,
    pub(super) version: &'a str,
    pub(super) ready: &'a Path,
    pub(super) external_version: Option<&'a Path>,
    pub(super) downloads: &'a Path,
    pub(super) hooks: &'a HookRunner,
    pub(super) app_root: &'a Path,
    pub(super) toolkit_root: &'a Path,
}

pub(super) fn commit(request: CommitRequest<'_>) -> Result<()> {
    let destination = &request.tool.install.destination;
    let version_destination = request.external_version.map(|_| {
        destination
            .parent()
            .expect("an installation destination always has a parent")
            .join(VERSION_FILE)
    });
    let mut installation =
        InstallationTransaction::begin(destination, version_destination.as_deref())?;
    if let Err(error) = installation.install(request.ready, request.external_version) {
        return Err(with_rollback(error, installation.rollback()));
    }

    let links = match LinkTransaction::install(request.tool) {
        Ok(links) => links,
        Err(error) => return Err(with_rollback(error, installation.rollback())),
    };
    let context = HookContext {
        app_root: request.app_root,
        toolkit_root: request.toolkit_root,
        downloads: request.downloads,
        staging: None,
        install: destination,
        version: Some(request.version),
    };
    if let Err(error) = request.hooks.run(
        &request.tool.hooks.after_install,
        HookStage::AfterInstall,
        request.tool,
        &context,
    ) {
        let link_rollback = links.rollback();
        let install_rollback = installation.rollback();
        return Err(with_rollbacks(error, [link_rollback, install_rollback]));
    }

    installation.finish()
}

fn combine_rollbacks(first: Option<Result<()>>, second: Option<Result<()>>) -> Result<()> {
    let failures = first
        .into_iter()
        .chain(second)
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

fn with_rollback(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    with_rollbacks(error, [rollback])
}

fn with_rollbacks<const N: usize>(
    error: anyhow::Error,
    rollbacks: [Result<()>; N],
) -> anyhow::Error {
    let failures = rollbacks
        .into_iter()
        .filter_map(Result::err)
        .map(|rollback| format!("{rollback:#}"))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        error
    } else {
        anyhow::anyhow!("{error:#}; rollback failed: {}", failures.join("; "))
    }
}
