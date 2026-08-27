use std::cmp::Ordering;

use anyhow::Result;

use crate::config::AppConfig;
use crate::domain::{UpdateResult, UpdateStatus};

use super::selection::validate_profiles;

pub(super) fn list_tools(config: &AppConfig, profiles: &[String]) -> Result<()> {
    validate_profiles(config, profiles)?;
    let mut tools = config
        .tools
        .values()
        .filter(|tool| profiles.is_empty() || profiles.contains(&tool.profile))
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| compare_tool_ids(&left.id, &right.id));

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

fn compare_tool_ids(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

pub(super) fn print_summary(results: &[UpdateResult]) {
    println!("\nUpdate summary");
    for result in results {
        tracing::debug!(
            tool = %result.tool_id,
            status = status_name(result.status),
            version = result.version.as_deref().unwrap_or("-"),
            message = %result.message,
            "update result"
        );
        println!(
            "  {:<32} {:<8} {:<20} {}",
            result.tool_id,
            status_name(result.status),
            result.version.as_deref().unwrap_or("-"),
            result.message
        );
    }
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

#[cfg(test)]
mod tests {
    use super::compare_tool_ids;

    #[test]
    fn tool_ids_are_sorted_case_insensitively() {
        let mut ids = ["zoxide", "BurpSuite", "afrog", "burpsuite"];
        ids.sort_by(|left, right| compare_tool_ids(left, right));

        assert_eq!(ids, ["afrog", "BurpSuite", "burpsuite", "zoxide"]);
    }
}
