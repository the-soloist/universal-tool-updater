use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::config::AppConfig;
use crate::domain::Tool;

pub(super) fn select_tools<'a>(
    config: &'a AppConfig,
    requested: &[String],
    profiles: &[String],
) -> Result<Vec<&'a Tool>> {
    for id in requested {
        if !config.tools.contains_key(id) {
            bail!("unknown tool id {id}");
        }
    }
    Ok(config
        .tools
        .values()
        .filter(|tool| requested.is_empty() || requested.contains(&tool.id))
        .filter(|tool| profiles.is_empty() || profiles.contains(&tool.profile))
        .collect())
}

pub(super) fn validate_profiles(config: &AppConfig, requested: &[String]) -> Result<()> {
    let available = config
        .tools
        .values()
        .map(|tool| tool.profile.as_str())
        .collect::<BTreeSet<_>>();
    for profile in requested {
        if !available.contains(profile.as_str()) {
            bail!("unknown profile {profile}");
        }
    }
    Ok(())
}
