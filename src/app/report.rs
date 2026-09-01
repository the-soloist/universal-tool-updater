use std::fmt::Write as _;

use crate::config::AppConfig;
use crate::display::{pad_right, sanitize_control_chars, width as display_width};
use crate::domain::{UpdateResult, UpdateStatus};
use anyhow::Result;
use console::{Style, Term};

use super::selection::validate_profiles;

pub(super) fn list_tools(config: &AppConfig, profiles: &[String]) -> Result<()> {
    validate_profiles(config, profiles)?;
    let mut tools = config
        .tools
        .values()
        .filter(|tool| profiles.is_empty() || profiles.contains(&tool.profile))
        .collect::<Vec<_>>();
    tools.sort_by_cached_key(|tool| list_sort_key(&tool.profile, &tool.name, &tool.id));

    for tool in tools {
        println!(
            "{:<32} {:<12} {}",
            tool.id,
            tool.profile,
            tool.install.destination.display()
        );
    }
    Ok(())
}

fn list_sort_key(profile: &str, name: &str, id: &str) -> (String, String, String) {
    (
        profile.to_lowercase(),
        name.to_lowercase(),
        id.to_lowercase(),
    )
}

pub(super) fn print_summary(results: &[UpdateResult]) {
    for result in results {
        tracing::debug!(
            tool = %result.tool_id,
            status = status_name(result.status),
            version = %sanitize_control_chars(result.version.as_deref().unwrap_or("-")),
            message = %sanitize_control_chars(&result.message),
            "update result"
        );
    }
    print!(
        "{}",
        render_summary(
            results,
            Term::stdout().size().1 as usize,
            console::colors_enabled(),
        )
    );
}

fn status_name(status: UpdateStatus) -> &'static str {
    match status {
        UpdateStatus::Updated => "updated",
        UpdateStatus::Current => "current",
        UpdateStatus::Skipped => "skipped",
        UpdateStatus::Failed => "failed",
        UpdateStatus::Planned => "planned",
    }
}

fn render_summary(results: &[UpdateResult], terminal_width: usize, color: bool) -> String {
    let terminal_width = terminal_width.max(40);
    let status_width = 9;
    let results = results
        .iter()
        .map(|result| {
            (
                result,
                sanitize_control_chars(result.version.as_deref().unwrap_or("-")),
                sanitize_control_chars(&result.message),
            )
        })
        .collect::<Vec<_>>();
    let desired_version_width = results
        .iter()
        .map(|(_, version, _)| display_width(version))
        .max()
        .unwrap_or(0)
        .max(display_width("VERSION"))
        .min(20);
    let version_width = desired_version_width.min((terminal_width / 4).clamp(7, 20));
    let maximum_tool_width = terminal_width
        .saturating_sub(15 + version_width)
        .max(display_width("TOOL"));
    let tool_width = results
        .iter()
        .map(|(result, _, _)| display_width(&result.tool_id))
        .max()
        .unwrap_or(0)
        .max(display_width("TOOL"))
        .min(32)
        .min(maximum_tool_width);
    let detail_offset = 2 + status_width + 2 + tool_width + 2 + version_width + 2;
    let detail_width = terminal_width.saturating_sub(detail_offset);
    let inline_details = detail_width >= 24;
    let header = Style::new().bold().dim().force_styling(color);
    let title = Style::new().bold().force_styling(color);
    let result_word = if results.len() == 1 {
        "result"
    } else {
        "results"
    };
    let mut output = String::new();

    writeln!(
        &mut output,
        "\n{}",
        title.apply_to(format!("Update summary · {} {result_word}", results.len()))
    )
    .expect("writing a String cannot fail");
    if inline_details {
        writeln!(
            &mut output,
            "  {}  {}  {}  {}",
            header.apply_to(pad_display("STATUS", status_width)),
            header.apply_to(pad_display("TOOL", tool_width)),
            header.apply_to(pad_display("VERSION", version_width)),
            header.apply_to("DETAIL")
        )
        .expect("writing a String cannot fail");
    } else {
        writeln!(
            &mut output,
            "  {}  {}  {}",
            header.apply_to(pad_display("STATUS", status_width)),
            header.apply_to(pad_display("TOOL", tool_width)),
            header.apply_to("VERSION")
        )
        .expect("writing a String cannot fail");
    }

    for (result, version, message) in results {
        let status = pad_display(status_label(result.status), status_width);
        let status = status_style(result.status, color).apply_to(status);
        let tool = pad_display(&result.tool_id, tool_width);
        let version = pad_display(&version, version_width);

        if inline_details {
            let lines = wrap_display(&message, detail_width);
            writeln!(
                &mut output,
                "  {status}  {tool}  {version}  {}",
                lines.first().map(String::as_str).unwrap_or_default()
            )
            .expect("writing a String cannot fail");
            for line in lines.iter().skip(1) {
                writeln!(&mut output, "{}{line}", " ".repeat(detail_offset))
                    .expect("writing a String cannot fail");
            }
        } else {
            writeln!(&mut output, "  {status}  {tool}  {version}")
                .expect("writing a String cannot fail");
            for line in wrap_display(&message, terminal_width.saturating_sub(4)) {
                writeln!(&mut output, "    {line}").expect("writing a String cannot fail");
            }
        }
    }
    output
}

fn status_label(status: UpdateStatus) -> &'static str {
    match status {
        UpdateStatus::Updated => "✓ updated",
        UpdateStatus::Current => "● current",
        UpdateStatus::Skipped => "○ skipped",
        UpdateStatus::Failed => "✗ failed",
        UpdateStatus::Planned => "→ planned",
    }
}

fn status_style(status: UpdateStatus, color: bool) -> Style {
    let style = match status {
        UpdateStatus::Updated => Style::new().green().bold(),
        UpdateStatus::Current => Style::new().cyan(),
        UpdateStatus::Skipped => Style::new().yellow(),
        UpdateStatus::Failed => Style::new().red().bold(),
        UpdateStatus::Planned => Style::new().magenta(),
    };
    style.force_styling(color)
}

fn pad_display(value: &str, width: usize) -> String {
    pad_right(value, width)
}

fn wrap_display(value: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        if !current.is_empty() && display_width(&current) + 1 + display_width(word) <= max_width {
            current.push(' ');
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }

        let mut remaining = word;
        while display_width(remaining) > max_width {
            let index = split_index(remaining, max_width);
            lines.push(remaining[..index].to_owned());
            remaining = &remaining[index..];
        }
        current.push_str(remaining);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_index(value: &str, max_width: usize) -> usize {
    let mut width = 0;
    let mut index = 0;
    for (offset, character) in value.char_indices() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_width && index != 0 {
            break;
        }
        width += character_width;
        index = offset + character.len_utf8();
        if width >= max_width {
            break;
        }
    }
    index.max(value.chars().next().map(char::len_utf8).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use crate::domain::{UpdateResult, UpdateStatus};

    use crate::display::width as display_width;

    use super::{list_sort_key, render_summary};

    #[test]
    fn tools_are_sorted_by_profile_then_name_case_insensitively() {
        let mut tools = [
            ("reverse", "zoxide", "zoxide"),
            ("Crypto", "CyberChef", "cyber-chef"),
            ("crypto", "captf-encoder", "captf-encoder"),
            ("reverse", "BurpSuite", "burp-suite"),
        ];
        tools.sort_by_cached_key(|(profile, name, id)| list_sort_key(profile, name, id));

        assert_eq!(
            tools,
            [
                ("crypto", "captf-encoder", "captf-encoder"),
                ("Crypto", "CyberChef", "cyber-chef"),
                ("reverse", "BurpSuite", "burp-suite"),
                ("reverse", "zoxide", "zoxide"),
            ]
        );
    }

    #[test]
    fn summary_uses_a_distinct_color_for_every_status() {
        let rendered = render_summary(&status_results(), 160, true);

        for ansi_color in ["\x1b[32m", "\x1b[36m", "\x1b[33m", "\x1b[31m", "\x1b[35m"] {
            assert!(rendered.contains(ansi_color), "missing {ansi_color:?}");
        }
    }

    #[test]
    fn summary_is_plain_text_when_color_is_disabled() {
        let rendered = render_summary(&status_results(), 100, false);

        assert!(!rendered.contains("\x1b["));
        assert!(rendered.contains("✓ updated"));
        assert!(rendered.contains("✗ failed"));
    }

    #[test]
    fn summary_strips_control_characters_from_remote_values() {
        let results = [UpdateResult {
            tool_id: "demo".to_owned(),
            status: UpdateStatus::Updated,
            version: Some("v\x1b[2J1.0".to_owned()),
            message: "done\x1b]0;pwned\u{7f}".to_owned(),
        }];

        let rendered = render_summary(&results, 100, false);

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\u{7f}'));
        assert!(rendered.contains("v[2J1.0"));
        assert!(rendered.contains("done"));
    }

    #[test]
    fn summary_wraps_long_details_to_the_terminal_width() {
        let results = [UpdateResult {
            tool_id: "d-beaver".to_owned(),
            status: UpdateStatus::Failed,
            version: None,
            message: "tool d-beaver: release resolution failed: no GitHub asset matched dbeaver-ce-very-long-version-win32.win32.x86_64.zip".to_owned(),
        }];

        let rendered = render_summary(&results, 64, false);

        assert!(rendered.lines().all(|line| display_width(line) <= 64));
        assert!(rendered.contains("d-beaver"));
    }

    fn status_results() -> Vec<UpdateResult> {
        [
            UpdateStatus::Updated,
            UpdateStatus::Current,
            UpdateStatus::Skipped,
            UpdateStatus::Failed,
            UpdateStatus::Planned,
        ]
        .into_iter()
        .map(|status| UpdateResult {
            tool_id: super::status_name(status).to_owned(),
            status,
            version: Some("v0.1.0".to_owned()),
            message: "example result".to_owned(),
        })
        .collect()
    }
}
