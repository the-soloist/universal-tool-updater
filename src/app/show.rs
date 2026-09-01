use std::env;
use std::path::{Component, Path};

use crate::config::AppConfig;
use crate::display::truncate;
use crate::domain::{InputMode, ReleaseConfig, Tool};
use anyhow::Result;
use console::Term;

use super::selection::validate_profiles;

mod table;

use table::MergedTable;

pub(super) fn show_distribution(config: &AppConfig, profiles: &[String]) -> Result<()> {
    print!(
        "{}",
        render_distribution(config, profiles, terminal_width())?
    );
    Ok(())
}

fn render_distribution(
    config: &AppConfig,
    profiles: &[String],
    available_width: usize,
) -> Result<String> {
    validate_profiles(config, profiles)?;
    let mut entries = config
        .tools
        .values()
        .filter(|tool| profiles.is_empty() || profiles.contains(&tool.profile))
        .map(|tool| DistributionEntry::new(config.paths.toolkit_root.as_path(), tool))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.profile.cmp(&right.profile).then_with(|| {
            left.directories
                .cmp(&right.directories)
                .then_with(|| left.tool.cmp(&right.tool))
        })
    });

    let directory_depth = entries
        .iter()
        .map(|entry| entry.directories.len())
        .max()
        .unwrap_or(0);
    let shown_directories = directory_columns(directory_depth, available_width);
    let headers = headers(directory_depth, shown_directories);
    let rows = entries
        .iter()
        .map(|entry| entry.cells(directory_depth, shown_directories))
        .collect::<Vec<_>>();
    let table = MergedTable::new(headers, rows, available_width);

    let title = format!("工具分布 · {} 个工具", entries.len());
    let mut output = format!("{}\n", fit_cell(&title, available_width));
    output.push_str(&table.render());
    Ok(output)
}

#[derive(Debug)]
struct DistributionEntry {
    profile: String,
    directories: Vec<String>,
    tool: String,
    manual: bool,
}

impl DistributionEntry {
    fn new(toolkit_root: &Path, tool: &Tool) -> Self {
        let destination = logical_destination(tool);
        let (external, path) = match destination.strip_prefix(toolkit_root) {
            Ok(relative) => (false, relative),
            Err(_) => (true, destination),
        };
        let mut directories = Vec::new();
        if external {
            directories.push("<external>".to_owned());
        }
        directories.extend(path.components().filter_map(component_label));

        Self {
            profile: tool.profile.clone(),
            directories,
            tool: tool.name.clone(),
            manual: matches!(tool.release, ReleaseConfig::Manual {}),
        }
    }

    fn cells(&self, directory_depth: usize, shown_directories: usize) -> Vec<String> {
        let mut cells = Vec::with_capacity(shown_directories + 2);
        cells.push(self.profile.clone());
        if shown_directories == directory_depth {
            cells.extend(self.directories.iter().cloned());
            cells.resize(shown_directories + 1, String::new());
        } else if shown_directories > 0 {
            let prefix_length = shown_directories - 1;
            cells.extend(self.directories.iter().take(prefix_length).cloned());
            cells.resize(shown_directories, String::new());
            cells.push(
                self.directories
                    .iter()
                    .skip(prefix_length)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
        cells.push(if self.manual {
            format!("{} [manual]", self.tool)
        } else {
            self.tool.clone()
        });
        cells
    }
}

fn logical_destination(tool: &Tool) -> &Path {
    let destination = tool.install.destination.as_path();
    if tool.install.input == InputMode::Copy
        && destination
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("release"))
    {
        destination.parent().unwrap_or(destination)
    } else {
        destination
    }
}

fn component_label(component: Component<'_>) -> Option<String> {
    match component {
        Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().into_owned()),
        Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
        Component::ParentDir => Some("..".to_owned()),
        Component::RootDir | Component::CurDir => None,
    }
}

fn headers(directory_depth: usize, shown_directories: usize) -> Vec<String> {
    let mut headers = Vec::with_capacity(shown_directories + 2);
    headers.push("Profile".to_owned());
    for level in 1..=shown_directories {
        if shown_directories < directory_depth && level == shown_directories {
            headers.push("路径".to_owned());
        } else {
            headers.push(format!("目录 {level}"));
        }
    }
    headers.push("工具".to_owned());
    headers
}

fn directory_columns(directory_depth: usize, available_width: usize) -> usize {
    if directory_depth == 0 {
        return 0;
    }
    (1..=directory_depth)
        .rev()
        .find(|columns| desired_table_width(*columns) <= available_width)
        .unwrap_or(1)
}

fn desired_table_width(directory_columns: usize) -> usize {
    let columns = directory_columns + 2;
    1 + columns * 3 + 7 + directory_columns * 6 + 8
}

fn fit_cell(value: &str, width: usize) -> String {
    truncate(value, width)
}

fn terminal_width() -> usize {
    Term::stdout()
        .size_checked()
        .map(|(_, columns)| usize::from(columns))
        .or_else(|| {
            env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
        })
        .filter(|width| *width > 0)
        .unwrap_or(120)
        .max(20)
}

#[cfg(test)]
mod tests;
