use crate::safe_fs::SafeRoot;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub schema_version: u32,
    pub version: String,
    pub commit_id: String,
    pub minecraft_version: String,
    pub neoforge_version: String,
}

#[derive(Debug)]
pub struct ComponentCheck {
    pub info: ReleaseInfo,
    pub base_pack_hash: [u8; 32],
    pub target_pack_hash: [u8; 32],
}

#[derive(Debug, Deserialize)]
struct PackDescriptor {
    versions: Components,
}

#[derive(Debug, Deserialize)]
struct Components {
    minecraft: String,
    neoforge: String,
}

pub fn check_components(
    base_tree: &Path,
    target_tree: &Path,
    instance: &Path,
) -> Result<ComponentCheck> {
    let base = SafeRoot::open(base_tree)?;
    let target = SafeRoot::open(target_tree)?;
    let instance = SafeRoot::open(instance)?;
    check_components_roots(&base, &target, &instance)
}

pub fn check_components_roots(
    base_tree: &SafeRoot,
    target_tree: &SafeRoot,
    instance: &SafeRoot,
) -> Result<ComponentCheck> {
    let info = load_release_info(instance)?;
    let descriptors = [
        ("base", base_tree.required_file("pack/pack.toml")?),
        ("target", target_tree.required_file("pack/pack.toml")?),
    ];
    for (label, bytes) in &descriptors {
        let descriptor: PackDescriptor =
            toml::from_str(std::str::from_utf8(bytes).context("pack/pack.toml is not UTF-8")?)
                .context("parsing pack/pack.toml")?;
        if descriptor.versions.minecraft != info.minecraft_version
            || descriptor.versions.neoforge != info.neoforge_version
        {
            bail!(
                "{label} changes Minecraft/NeoForge ({} / {}); create a new instance instead",
                descriptor.versions.minecraft,
                descriptor.versions.neoforge
            );
        }
    }
    let [base_pack_hash, target_pack_hash] =
        descriptors.map(|(_, bytes)| Sha256::digest(bytes).into());
    Ok(ComponentCheck {
        info,
        base_pack_hash,
        target_pack_hash,
    })
}

pub fn load_release_info(instance: &SafeRoot) -> Result<ReleaseInfo> {
    let info: ReleaseInfo =
        serde_json::from_slice(&instance.required_file("config/createdelight_release_info.json")?)
            .context("parsing installed release marker")?;
    if info.schema_version != 1
        || info.version.trim().is_empty()
        || info.commit_id.len() != 40
        || !info.commit_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || info.minecraft_version.trim().is_empty()
        || info.neoforge_version.trim().is_empty()
    {
        bail!("invalid installed release marker");
    }
    Ok(info)
}
