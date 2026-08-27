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
    use super::list_sort_key;

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
}
