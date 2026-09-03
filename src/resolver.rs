mod github;
mod http;
mod util;
mod web;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::redirect;
use url::Url;

use crate::domain::{NetworkConfig, ReleaseConfig, ResolvedRelease, Tool};

pub struct Resolver {
    client: Client,
    github_token: Option<String>,
    allow_insecure_transports: bool,
}

impl Resolver {
    pub fn new(settings: &NetworkConfig, allow_insecure_transports: bool) -> Result<Self> {
        let client = Client::builder()
            .user_agent(&settings.user_agent)
            .timeout(Duration::from_secs(settings.timeout_seconds))
            .redirect(transport_policy(allow_insecure_transports))
            .build()
            .context("cannot create HTTP client")?;
        let github_token = std::env::var(&settings.github_token_env)
            .ok()
            .filter(|token| !token.trim().is_empty());
        Ok(Self {
            client,
            github_token,
            allow_insecure_transports,
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
            ReleaseConfig::Manual {} => Err(anyhow::anyhow!(
                "tool {} is maintained manually and has no release to resolve",
                tool.id
            )),
        }
    }
}

/// 共享 client 的重定向策略：默认拒绝 HTTPS→HTTP 降级，opt-in 后放行；
/// 其余跳转沿用默认限制（同链最多 10 跳，超出报错）。
fn transport_policy(allow_insecure_transports: bool) -> redirect::Policy {
    let default = redirect::Policy::default();
    redirect::Policy::custom(move |attempt| {
        match transport_downgrade(attempt.url(), attempt.previous(), allow_insecure_transports) {
            Some(from) => {
                let next = attempt.url().clone();
                let message = format!(
                    "redirect from {from} to {next} would downgrade an HTTPS request to plaintext \
                     HTTP; set allow_insecure_transports: true in the manifest to permit it"
                );
                attempt.error(message)
            }
            None => default.redirect(attempt),
        }
    })
}

/// 返回链路中被降级的 HTTPS 来源；next 非 http、已显式放行或链路中没有 https 时为 None。
fn transport_downgrade<'a>(
    next: &'a Url,
    previous: &'a [Url],
    allow_insecure_transports: bool,
) -> Option<&'a Url> {
    if allow_insecure_transports || next.scheme() != "http" {
        return None;
    }
    previous.iter().rev().find(|url| url.scheme() == "https")
}

#[cfg(test)]
mod tests {
    use super::transport_downgrade;
    use url::Url;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn blocks_https_to_http_redirect_downgrades_by_default() {
        let previous = [url("https://example.com/releases")];
        let next = url("http://mirror.example.com/tool.zip");
        let downgrade = transport_downgrade(&next, &previous, false);
        assert_eq!(downgrade, Some(&url("https://example.com/releases")));
    }

    #[test]
    fn allows_http_and_upgrade_redirects_without_opt_in() {
        let http_origin = [url("http://example.com/page")];
        assert_eq!(
            transport_downgrade(&url("http://example.com/tool.zip"), &http_origin, false),
            None
        );
        assert_eq!(
            transport_downgrade(&url("https://example.com/tool.zip"), &http_origin, false),
            None
        );
        let https_origin = [url("https://example.com/page")];
        assert_eq!(
            transport_downgrade(
                &url("https://cdn.example.com/tool.zip"),
                &https_origin,
                false
            ),
            None
        );
    }

    #[test]
    fn allows_https_to_http_downgrades_only_when_opted_in() {
        let previous = [url("https://example.com/releases")];
        assert_eq!(
            transport_downgrade(&url("http://example.com/tool.zip"), &previous, true),
            None
        );
    }

    #[test]
    fn flags_the_https_origin_of_a_downgrade_across_a_mixed_chain() {
        let previous = [
            url("https://example.com/start"),
            url("http://middle.example/hop"),
        ];
        let next = url("http://cdn.example.com/tool.zip");
        let downgrade = transport_downgrade(&next, &previous, false);
        assert_eq!(downgrade, Some(&url("https://example.com/start")));
    }
}
