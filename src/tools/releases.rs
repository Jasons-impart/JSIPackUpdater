use super::*;
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRelease {
    pub tool: Tool,
    pub version: String,
    pub platform: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub minimum_updater: Option<String>,
}

impl ToolRelease {
    pub fn validate(&self) -> Result<()> {
        let version = normalized_version(&self.version)?;
        if self.platform != platform()? {
            bail!("tool release has the wrong platform");
        }
        let asset = format!(
            "{}-v{version}-{}{}",
            self.tool.name(),
            self.platform,
            if cfg!(windows) { ".exe" } else { "" }
        );
        let expected = format!(
            "https://github.com/{}/releases/download/v{version}/{asset}",
            self.tool.repository()
        );
        if self.url != expected
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("invalid tool release location or checksum");
        }
        if let Some(required) = &self.minimum_updater {
            normalized_version(required)?;
        }
        Ok(())
    }
    pub fn supported(&self) -> Result<()> {
        self.validate()?;
        if let Some(required) = &self.minimum_updater
            && newer(required, env!("CARGO_PKG_VERSION"))?
        {
            bail!("this tool needs updater {required}; update minecraft-modpack-updater first");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    schema_version: u32,
    releases: Vec<ToolRelease>,
}

pub(super) fn client() -> Result<Client> {
    Ok(Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .user_agent("minecraft-modpack-updater")
        .build()?)
}

pub(crate) fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let response = client()?.get(url).send()?.error_for_status()?;
    if response.content_length().is_some_and(|n| n > limit) {
        bail!("tool download is too large");
    }
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("tool download is too large");
    }
    Ok(bytes)
}

fn github_release(tool: Tool) -> Result<Option<Release>> {
    let mut request = client()?.get(format!(
        "https://api.github.com/repos/{}/releases/latest",
        tool.repository()
    ));
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.bearer_auth(token);
    }
    let response = request.send()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    response
        .error_for_status()?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        bail!("tool release metadata is too large");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Prefer the updater's published catalog; official Release metadata bootstraps
/// tool acquisition before the first updater catalog has been published.
pub fn latest(tool: Tool) -> Result<ToolRelease> {
    let updater = github_release(Tool::Updater)?;
    if let Some(release) = &updater
        && let Some(asset) = release
            .assets
            .iter()
            .find(|a| a.name == "tool-releases.json")
    {
        let tag = normalized_version(&release.tag_name)?;
        let expected = format!(
            "https://github.com/{}/releases/download/v{tag}/tool-releases.json",
            Tool::Updater.repository()
        );
        if asset.browser_download_url != expected {
            bail!("invalid tool catalog location");
        }
        let catalog: Catalog = serde_json::from_slice(&download(&expected, 1024 * 1024)?)?;
        if catalog.schema_version != 1 {
            bail!("unsupported tool catalog schema");
        }
        let target = platform()?;
        let mut matching = catalog
            .releases
            .into_iter()
            .filter(|r| r.tool == tool && r.platform == target);
        let selected = matching
            .next()
            .context("no compatible platform entry in the tool catalog")?;
        if matching.next().is_some() {
            bail!("duplicate platform entry in tool catalog");
        }
        selected.validate()?;
        return Ok(selected);
    }
    let release = if tool == Tool::Updater {
        updater
    } else {
        github_release(tool)?
    }
    .context("this tool has no published release yet")?;
    let version = normalized_version(&release.tag_name)?;
    let target = platform()?;
    let asset = format!(
        "{}-v{version}-{target}{}",
        tool.name(),
        if cfg!(windows) { ".exe" } else { "" }
    );
    let url = format!(
        "https://github.com/{}/releases/download/v{version}/{asset}",
        tool.repository()
    );
    if !release
        .assets
        .iter()
        .any(|a| a.name == asset && a.browser_download_url == url)
    {
        bail!("this tool release has no binary for {target}");
    }
    let checksum_url = format!("{url}.sha256");
    if !release
        .assets
        .iter()
        .any(|a| a.name == format!("{asset}.sha256") && a.browser_download_url == checksum_url)
    {
        bail!("this tool release has no published checksum");
    }
    let sum = download(&checksum_url, 4096)?;
    let sha256 = std::str::from_utf8(&sum)?
        .split_whitespace()
        .next()
        .context("empty tool checksum")?
        .to_owned();
    let selected = ToolRelease {
        tool,
        version,
        platform: target.into(),
        url,
        sha256,
        minimum_updater: None,
    };
    selected.validate()?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_cannot_switch_repository_platform_or_executable_identity() {
        let target = platform().unwrap();
        let mut r = ToolRelease {
            tool: Tool::Bkmpw,
            version: "0.2.5".into(),
            platform: target.into(),
            url: format!(
                "https://github.com/{}/releases/download/v0.2.5/bkmpw-v0.2.5-{target}{}",
                Tool::Bkmpw.repository(),
                if cfg!(windows) { ".exe" } else { "" }
            ),
            sha256: "a".repeat(64),
            minimum_updater: None,
        };
        r.validate().unwrap();
        r.minimum_updater = Some("99.0.0".into());
        assert!(r.supported().is_err());
        r.minimum_updater = None;
        r.url = r.url.replace(Tool::Bkmpw.repository(), "unknown/repo");
        assert!(r.validate().is_err());
        r.tool = Tool::Updater;
        assert!(r.validate().is_err());
    }
}
