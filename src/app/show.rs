use std::cmp;
use std::env;
use std::path::{Component, Path};

use anyhow::Result;
use console::Term;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::AppConfig;
use crate::domain::{InputMode, Tool};

use super::selection::validate_profiles;

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
        cells.push(self.tool.clone());
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

#[derive(Debug)]
struct MergedTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    widths: Vec<usize>,
}

impl MergedTable {
    fn new(headers: Vec<String>, rows: Vec<Vec<String>>, available_width: usize) -> Self {
        let natural_widths = headers
            .iter()
            .enumerate()
            .map(|(column, header)| {
                rows.iter().fold(display_width(header), |width, row| {
                    cmp::max(width, display_width(&row[column]))
                })
            })
            .collect::<Vec<_>>();
        let widths = fit_widths(&natural_widths, available_width);
        Self {
            headers,
            rows,
            widths,
        }
    }

    fn render(&self) -> String {
        let mut output = String::new();
        self.render_full_border(&mut output, '┌', '┬', '┐');
        self.render_row(&mut output, &self.headers, None);
        self.render_full_border(&mut output, '├', '┼', '┤');
        for (index, row) in self.rows.iter().enumerate() {
            let continuations = index
                .checked_sub(1)
                .map(|previous| merged_columns(&self.rows[previous], row));
            self.render_row(&mut output, row, continuations.as_deref());
            if index + 1 < self.rows.len() {
                let next = merged_columns(row, &self.rows[index + 1]);
                self.render_merged_border(&mut output, &next);
            }
        }
        self.render_full_border(&mut output, '└', '┴', '┘');
        output
    }

    fn render_row(&self, output: &mut String, cells: &[String], merged: Option<&[bool]>) {
        output.push('│');
        for (index, cell) in cells.iter().enumerate() {
            let hidden = merged.is_some_and(|columns| columns[index]);
            let value = fit_cell(if hidden { "" } else { cell }, self.widths[index]);
            output.push(' ');
            output.push_str(&value);
            output.extend(std::iter::repeat_n(
                ' ',
                self.widths[index] - display_width(&value) + 1,
            ));
            output.push('│');
        }
        output.push('\n');
    }

    fn render_full_border(&self, output: &mut String, left: char, middle: char, right: char) {
        output.push(left);
        for (index, width) in self.widths.iter().enumerate() {
            output.extend(std::iter::repeat_n('─', width + 2));
            output.push(if index + 1 == self.widths.len() {
                right
            } else {
                middle
            });
        }
        output.push('\n');
    }

    fn render_merged_border(&self, output: &mut String, continuations: &[bool]) {
        output.push(if continuations[0] { '│' } else { '├' });
        for (index, width) in self.widths.iter().enumerate() {
            let horizontal = !continuations[index];
            output.extend(std::iter::repeat_n(
                if horizontal { '─' } else { ' ' },
                width + 2,
            ));
            let junction = if index + 1 == self.widths.len() {
                if horizontal { '┤' } else { '│' }
            } else {
                match (horizontal, !continuations[index + 1]) {
                    (true, true) => '┼',
                    (true, false) => '┤',
                    (false, true) => '├',
                    (false, false) => '│',
                }
            };
            output.push(junction);
        }
        output.push('\n');
    }
}

fn merged_columns(previous: &[String], current: &[String]) -> Vec<bool> {
    let last = current.len().saturating_sub(1);
    let mut same_parent = true;
    current
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            same_parent &= previous[index] == *cell;
            same_parent && !cell.is_empty() && index != last
        })
        .collect()
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn fit_widths(natural: &[usize], available_width: usize) -> Vec<usize> {
    let frame_width = natural.len() * 3 + 1;
    let content_budget = available_width
        .saturating_sub(frame_width)
        .max(natural.len());
    let mut widths = vec![1; natural.len()];
    let mut remaining = content_budget.saturating_sub(natural.len());
    let last = natural.len() - 1;
    let mut order = vec![last, 0];
    order.extend(1..last);

    let desired = natural
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let minimum = if index == 0 {
                7
            } else if index == last {
                8
            } else {
                6
            };
            cmp::min(*width, minimum)
        })
        .collect::<Vec<_>>();
    grow_widths(&mut widths, &desired, &order, &mut remaining);
    grow_widths(&mut widths, natural, &order, &mut remaining);
    widths
}

fn grow_widths(widths: &mut [usize], targets: &[usize], order: &[usize], remaining: &mut usize) {
    while *remaining > 0 {
        let mut changed = false;
        for index in order {
            if *remaining == 0 {
                break;
            }
            if widths[*index] < targets[*index] {
                widths[*index] += 1;
                *remaining -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn fit_cell(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width - 1;
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
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
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crate::config::{AppConfig, Paths};
    use crate::domain::{
        ArtifactConfig, HookConfig, InputMode, InstallSpec, NetworkConfig, ReleaseConfig, Tool,
    };

    use super::{display_width, merged_columns, render_distribution};

    fn tool(
        id: &str,
        name: &str,
        profile: &str,
        destination: &str,
        input: InputMode,
        enabled: bool,
    ) -> Tool {
        Tool {
            id: id.to_owned(),
            name: name.to_owned(),
            profile: profile.to_owned(),
            enabled,
            release: ReleaseConfig::Github {
                repository: "owner/repository".to_owned(),
                ignore_versions: Vec::new(),
            },
            artifacts: vec![ArtifactConfig::GithubAsset {
                pattern: ".*".to_owned(),
            }],
            install: InstallSpec {
                destination: PathBuf::from("/toolkit").join(destination),
                input,
                existing: Default::default(),
                save: Default::default(),
                strip_single_root: true,
                create_destination: true,
                archive_name: "{name} - {version}.7z".to_owned(),
                archive_password: None,
                executable: Vec::new(),
                symlinks: Vec::new(),
            },
            hooks: HookConfig::default(),
        }
    }

    fn config() -> AppConfig {
        let tools = [
            tool(
                "nuclei",
                "Nuclei",
                "web",
                "Web/扫描器/Nuclei",
                InputMode::Extract,
                true,
            ),
            tool(
                "fscan",
                "fscan",
                "web",
                "Web/扫描器/fscan/release",
                InputMode::Copy,
                true,
            ),
            tool(
                "jadx",
                "JADX",
                "reverse",
                "Reverse/Decompiler/JADX",
                InputMode::Extract,
                false,
            ),
        ]
        .into_iter()
        .map(|tool| (tool.id.clone(), tool))
        .collect::<BTreeMap<_, _>>();
        AppConfig {
            app_root: PathBuf::from("/app"),
            paths: Paths {
                toolkit_root: PathBuf::from("/toolkit"),
                downloads: PathBuf::from("/toolkit/updates"),
                staging: PathBuf::from("/toolkit/updates/staging"),
                state: PathBuf::from("/toolkit/.updater/state.yaml"),
            },
            network: NetworkConfig::default(),
            tools,
        }
    }

    #[test]
    fn renders_a_unicode_aware_table_with_merged_hierarchy_cells() {
        let rendered = render_distribution(&config(), &[], 120).unwrap();
        assert!(rendered.starts_with("工具分布 · 3 个工具\n┌"));
        assert_eq!(rendered.matches("web").count(), 1);
        assert_eq!(rendered.matches("reverse").count(), 1);
        assert_eq!(rendered.matches("Web").count(), 1);
        assert_eq!(rendered.matches("扫描器").count(), 1);
        assert!(rendered.contains("JADX"));
        assert!(!rendered.contains("(jadx)"));
        assert!(!rendered.contains("[disabled]"));
        assert!(!rendered.contains("release"));
        assert_eq!(display_width("扫描器"), 6);
    }

    #[test]
    fn adapts_columns_and_content_to_the_terminal_width() {
        let width = 40;
        let rendered = render_distribution(&config(), &[], width).unwrap();
        assert!(rendered.contains("│ 路径"));
        assert!(rendered.contains('…'));
        for line in rendered.lines() {
            assert!(
                display_width(line) <= width,
                "line is wider than {width} columns: {line}"
            );
        }
    }

    #[test]
    fn marks_only_equal_non_leaf_prefixes_as_merged() {
        let previous = ["web", "Web", "扫描器", "Nuclei"]
            .map(ToOwned::to_owned)
            .to_vec();
        let current = ["web", "Web", "扫描器", "fscan"]
            .map(ToOwned::to_owned)
            .to_vec();
        assert_eq!(
            merged_columns(&previous, &current),
            vec![true, true, true, false]
        );
    }

    #[test]
    fn filters_the_distribution_by_profile() {
        let rendered = render_distribution(&config(), &["reverse".to_owned()], 120).unwrap();
        assert!(rendered.starts_with("工具分布 · 1 个工具\n"));
        assert!(rendered.contains("JADX"));
        assert!(!rendered.contains("Web"));
    }
}
