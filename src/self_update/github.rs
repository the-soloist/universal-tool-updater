use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use semver::Version;
use serde::Deserialize;
use url::Url;

const REPOSITORY: &str = "the-soloist/universal-tool-updater";

#[derive(Debug)]
pub(super) struct Release {
    pub(super) tag: String,
    pub(super) version: Version,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    pub(super) fn asset_url(&self, name: &str) -> Result<&str> {
        let mut matches = self.assets.iter().filter(|asset| asset.name == name);
        let asset = matches
            .next()
            .with_context(|| format!("release {} does not contain asset {name:?}", self.tag))?;
        if matches.next().is_some() {
            bail!(
                "release {} contains duplicate assets named {name:?}",
                self.tag
            );
        }
        let url = Url::parse(&asset.browser_download_url).with_context(|| {
            format!(
                "release {} contains an invalid download URL for {name:?}",
                self.tag
            )
        })?;
        let expected_prefix = format!("/{REPOSITORY}/releases/download/{}/", self.tag);
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || !url.path().starts_with(&expected_prefix)
        {
            bail!(
                "release {} contains an unexpected download URL for {name:?}",
                self.tag
            );
        }
        Ok(&asset.browser_download_url)
    }
}

pub(super) fn latest(client: &Client, token: Option<&str>) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let mut request = client
        .get(&url)
        .header(ACCEPT, "application/vnd.github+json");
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let release: ApiRelease = request
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .context("cannot query the latest updater release from GitHub")?
        .json()
        .context("GitHub returned an invalid updater release response")?;
    if release.draft || release.prerelease {
        bail!("GitHub returned a draft or prerelease as the latest stable updater release");
    }
    let raw_version = release
        .tag_name
        .strip_prefix('v')
        .context("latest updater release tag must start with 'v'")?;
    let version = Version::parse(raw_version).with_context(|| {
        format!(
            "latest updater release tag {:?} is not semver",
            release.tag_name
        )
    })?;
    if release.tag_name != format!("v{version}") {
        bail!(
            "latest updater release tag {:?} is not in canonical v<semver> form",
            release.tag_name
        );
    }
    Ok(Release {
        tag: release.tag_name,
        version,
        assets: release.assets,
    })
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::{Asset, Release};

    #[test]
    fn requires_one_exact_asset_name() {
        let release = Release {
            tag: "v1.2.3".to_owned(),
            version: Version::new(1, 2, 3),
            assets: vec![Asset {
                name: "updater-v1.2.3-linux-x86_64.7z".to_owned(),
                browser_download_url: "https://github.com/the-soloist/universal-tool-updater/releases/download/v1.2.3/updater-v1.2.3-linux-x86_64.7z".to_owned(),
            }],
        };
        assert_eq!(
            release.asset_url("updater-v1.2.3-linux-x86_64.7z").unwrap(),
            "https://github.com/the-soloist/universal-tool-updater/releases/download/v1.2.3/updater-v1.2.3-linux-x86_64.7z"
        );
        assert!(release.asset_url("updater-v1.2.3-macos-arm64.7z").is_err());
    }
}
