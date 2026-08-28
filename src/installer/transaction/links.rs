use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::domain::{SymlinkSpec, Tool};
use crate::error::UpdaterError;

use super::super::filesystem::{create_file_symlink, remove_file_symlink};
use super::with_rollback;

struct LinkChange {
    target: PathBuf,
    previous: Option<PathBuf>,
}

pub(super) struct LinkTransaction {
    changes: Vec<LinkChange>,
}

impl LinkTransaction {
    pub(super) fn install(tool: &Tool) -> Result<Self> {
        validate_sources(tool)?;
        let mut transaction = Self {
            changes: Vec::with_capacity(tool.install.symlinks.len()),
        };
        for link in &tool.install.symlinks {
            if let Err(error) = transaction.install_one(tool, link) {
                return Err(with_rollback(error, transaction.rollback()));
            }
        }
        Ok(transaction)
    }

    fn install_one(&mut self, tool: &Tool, link: &SymlinkSpec) -> Result<()> {
        if let Some(parent) = link.to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create symlink parent {}", parent.display()))?;
        }
        let previous = take_existing_link(tool, &link.to)?;
        self.changes.push(LinkChange {
            target: link.to.clone(),
            previous,
        });
        let source = tool.install.destination.join(&link.from);
        create_file_symlink(&source, &link.to)
            .with_context(|| format!("cannot create symlink {}", link.to.display()))
    }

    pub(super) fn rollback(self) -> Result<()> {
        let mut failures = Vec::new();
        for change in self.changes.into_iter().rev() {
            if let Err(error) = remove_file_symlink(&change.target) {
                failures.push(format!(
                    "cannot remove symlink {}: {error:#}",
                    change.target.display()
                ));
                continue;
            }
            if let Some(previous) = change.previous
                && let Err(error) = create_file_symlink(&previous, &change.target)
            {
                failures.push(format!(
                    "cannot restore symlink {}: {error:#}",
                    change.target.display()
                ));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(failures.join("; "))
        }
    }
}

fn validate_sources(tool: &Tool) -> Result<()> {
    for link in &tool.install.symlinks {
        let source = tool.install.destination.join(&link.from);
        if !source.is_file() {
            return Err(UpdaterError::Installation {
                tool: tool.id.clone(),
                message: format!("symlink source {} is not a file", source.display()),
            }
            .into());
        }
    }
    Ok(())
}

fn take_existing_link(tool: &Tool, target: &std::path::Path) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let previous = fs::read_link(target)
                .with_context(|| format!("cannot read existing symlink {}", target.display()))?;
            fs::remove_file(target)
                .with_context(|| format!("cannot replace symlink {}", target.display()))?;
            Ok(Some(previous))
        }
        Ok(_) => Err(UpdaterError::Installation {
            tool: tool.id.clone(),
            message: format!("refusing to replace non-symlink {}", target.display()),
        }
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect symlink target {}", target.display())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::domain::SymlinkSpec;
    use crate::test_support::tool as test_tool;

    use super::LinkTransaction;

    #[test]
    fn rolls_back_earlier_links_when_a_later_link_fails() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("first"), "first").unwrap();
        fs::write(destination.join("second"), "second").unwrap();
        let old_source = directory.path().join("old");
        fs::write(&old_source, "old").unwrap();
        let first_target = directory.path().join("bin/first");
        fs::create_dir(first_target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&old_source, &first_target).unwrap();
        let blocked_parent = directory.path().join("blocked");
        fs::write(&blocked_parent, "not a directory").unwrap();

        let mut tool = test_tool("demo", destination);
        tool.install.symlinks = vec![
            SymlinkSpec {
                from: PathBuf::from("first"),
                to: first_target.clone(),
            },
            SymlinkSpec {
                from: PathBuf::from("second"),
                to: blocked_parent.join("second"),
            },
        ];

        assert!(LinkTransaction::install(&tool).is_err());
        assert_eq!(fs::read_link(first_target).unwrap(), old_source);
    }

    #[test]
    fn rollback_refuses_to_delete_a_replacement_regular_file() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("Demo");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("tool"), "tool").unwrap();
        let target = directory.path().join("bin/tool");

        let mut tool = test_tool("demo", destination);
        tool.install.symlinks = vec![SymlinkSpec {
            from: PathBuf::from("tool"),
            to: target.clone(),
        }];
        let transaction = LinkTransaction::install(&tool).unwrap();
        fs::remove_file(&target).unwrap();
        fs::write(&target, "replacement").unwrap();

        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "replacement");
        assert!(Path::new(&target).is_file());
    }
}
