mod github;
mod http;
mod util;
mod web;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client;

use crate::domain::{NetworkConfig, ReleaseConfig, ResolvedRelease, Tool};

pub struct Resolver {
    client: Client,
    github_token: Option<String>,
}

impl Resolver {
    pub fn new(settings: &NetworkConfig) -> Result<Self> {
        let client = Client::builder()
            .user_agent(&settings.user_agent)
            .timeout(Duration::from_secs(settings.timeout_seconds))
            .build()
            .context("cannot create HTTP client")?;
        let github_token = std::env::var(&settings.github_token_env)
            .ok()
            .filter(|token| !token.trim().is_empty());
        Ok(Self {
            client,
            github_token,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn resolve(&self, tool: &Tool) -> Result<ResolvedRelease> {
        match &tool.release {
            ReleaseConfig::Github {
                repository,
                ignore_versions,
            } => github::resolve(
                &self.client,
                self.github_token.as_deref(),
                tool,
                repository,
                ignore_versions,
            ),
            ReleaseConfig::Web {
                url,
                version_pattern,
                ignore_versions,
            } => web::resolve(&self.client, tool, url, version_pattern, ignore_versions),
            ReleaseConfig::Http {
                url,
                version_headers,
            } => http::resolve(&self.client, tool, url, version_headers),
        }
    }
}
