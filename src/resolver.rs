mod github;
mod http;
mod util;
mod web;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client;

use crate::domain::{NetworkConfig, ReleaseConfig, ResolvedRelease, Tool};

use self::github::HtmlAssetCache;

pub struct Resolver {
    client: Client,
    github_token: Option<String>,
    allow_insecure_transports: bool,
    html_assets: HtmlAssetCache,
}

impl Resolver {
    pub fn new(settings: &NetworkConfig, allow_insecure_transports: bool) -> Result<Self> {
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
            allow_insecure_transports,
            html_assets: HtmlAssetCache::default(),
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
                allow_prereleases,
            } => github::resolve(
                &self.client,
                self.github_token.as_deref(),
                &self.html_assets,
                tool,
                repository,
                ignore_versions,
                *allow_prereleases,
            ),
            ReleaseConfig::Web {
                url,
                version_pattern,
                ignore_versions,
            } => web::resolve(
                &self.client,
                tool,
                url,
                version_pattern,
                ignore_versions,
                self.allow_insecure_transports,
            ),
            ReleaseConfig::Http {
                url,
                version_headers,
            } => http::resolve(&self.client, tool, url, version_headers),
            // Manual tools are skipped before resolution in the update flow;
            // fail loudly if any other path tries to resolve them.
            ReleaseConfig::Manual {} => Err(anyhow::anyhow!(
                "tool {} is managed manually and has no release to resolve",
                tool.id
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Resolver;
    use crate::domain::{NetworkConfig, ReleaseConfig};
    use crate::test_support::tool;

    #[test]
    fn manual_tools_never_reach_release_resolution() {
        let mut manual = tool("ida-pro", "/toolkit/Reverse/IDA");
        manual.release = ReleaseConfig::Manual {};
        let resolver = Resolver::new(&NetworkConfig::default(), false).unwrap();

        let error = resolver.resolve(&manual).unwrap_err();
        assert!(
            error.to_string().contains("managed manually"),
            "unexpected error: {error:#}"
        );
    }
}
