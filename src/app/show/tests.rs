use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{AppConfig, Paths};
use crate::display::width as display_width;
use crate::domain::{ExtractionLimits, InputMode, NetworkConfig, ReleaseConfig, Tool};
use crate::test_support::tool as test_tool;

use super::render_distribution;
use super::table::merged_columns;

fn tool(
    id: &str,
    name: &str,
    profile: &str,
    destination: &str,
    input: InputMode,
    enabled: bool,
) -> Tool {
    let mut tool = test_tool(id, PathBuf::from("/toolkit").join(destination));
    tool.name = name.to_owned();
    tool.profile = profile.to_owned();
    tool.enabled = enabled;
    tool.install.input = input;
    tool
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
        allow_insecure_transports: false,
        extraction_limits: ExtractionLimits::default(),
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

#[test]
fn marks_manual_placeholders_in_the_tree() {
    let mut manual = tool(
        "ida-pro",
        "IDA Pro",
        "reverse",
        "Reverse/Decompiler/IDA Pro",
        InputMode::Extract,
        true,
    );
    manual.release = ReleaseConfig::Manual {};
    let managed = tool(
        "jadx",
        "JADX",
        "reverse",
        "Reverse/Decompiler/JADX",
        InputMode::Extract,
        true,
    );
    let tools = [manual, managed]
        .into_iter()
        .map(|tool| (tool.id.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let config = AppConfig {
        app_root: PathBuf::from("/app"),
        paths: Paths {
            toolkit_root: PathBuf::from("/toolkit"),
            downloads: PathBuf::from("/toolkit/updates"),
            staging: PathBuf::from("/toolkit/updates/staging"),
            state: PathBuf::from("/toolkit/.updater/state.yaml"),
        },
        network: NetworkConfig::default(),
        allow_insecure_transports: false,
        extraction_limits: ExtractionLimits::default(),
        tools,
    };

    let rendered = render_distribution(&config, &[], 120).unwrap();

    assert!(rendered.starts_with("工具分布 · 2 个工具\n"));
    assert!(rendered.contains("IDA Pro [manual]"));
    assert!(rendered.contains("JADX"));
    assert!(!rendered.contains("JADX [manual]"));
}
