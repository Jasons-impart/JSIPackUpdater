use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod process;
mod releases;
mod state;
pub use releases::{ToolRelease, latest};
pub use state::{current_updater, discover, save};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Bkmpw,
    Updater,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bkmpw => "bkmpw",
            Self::Updater => "minecraft-modpack-updater",
        }
    }
    pub fn package(self) -> &'static str {
        match self {
            Self::Bkmpw => "@bro-know-my/packwiz",
            Self::Updater => "@jsi-team/minecraft-modpack-updater",
        }
    }
    pub fn repository(self) -> &'static str {
        match self {
            Self::Bkmpw => "bro-know-my-org/BroKnowMyPackwiz",
            Self::Updater => "Jasons-impart/JSIPackUpdater",
        }
    }
    pub fn executable(self) -> String {
        format!("{}{}", self.name(), if cfg!(windows) { ".exe" } else { "" })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum InstallChoice {
    Local,
    Npm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    Local,
    Npm,
    External,
    Portable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub tool: Tool,
    pub version: String,
    pub ownership: Ownership,
    pub path: PathBuf,
    pub sha256: String,
}

pub fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!("no tools are published for this platform"),
    }
}

pub fn normalized_version(version: &str) -> Result<String> {
    let version = version.trim_start_matches('v');
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 8 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        bail!("invalid stable tool version: {version}");
    }
    Ok(parts
        .iter()
        .map(|p| p.parse::<u32>().map(|v| v.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("."))
}

pub fn newer(candidate: &str, installed: &str) -> Result<bool> {
    let parts = |v: &str| -> Result<Vec<u32>> {
        Ok(normalized_version(v)?
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?)
    };
    Ok(parts(candidate)? > parts(installed)?)
}

pub fn npm_available() -> bool {
    process::npm(&["--version"]).is_ok()
}

pub fn npm_native_path(tool: Tool) -> Result<PathBuf> {
    process::npm_binary(tool)
}

pub fn inspect_updater(path: &Path) -> Result<String> {
    let mut command = std::process::Command::new(path);
    command.arg("--version");
    let output = process::capture(command)?;
    normalized_version(
        std::str::from_utf8(&output)?
            .trim()
            .strip_prefix("minecraft-modpack-updater ")
            .context("unexpected updater version response")?,
    )
}
