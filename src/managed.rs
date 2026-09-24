use crate::planner::validate_source_paths;
use crate::safe_fs::{ArtifactHashes, LocalArtifact, SafeRoot, portable_case_key};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Client,
    Server,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedFile {
    pub name: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub side: Side,
    pub files: Vec<ManagedFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdoptionConflict {
    pub path: String,
    pub reason: &'static str,
}

#[derive(Debug)]
pub struct Adoption {
    pub manifest: Manifest,
    pub conflicts: Vec<AdoptionConflict>,
}

#[derive(Deserialize)]
struct Metadata {
    name: String,
    filename: String,
    side: Option<String>,
    download: Option<Download>,
}

#[derive(Deserialize)]
struct Download {
    #[serde(rename = "hash-format")]
    hash_format: Option<String>,
    hash: Option<String>,
}

fn applies(metadata_path: &str, metadata: &Metadata, side: Side) -> Result<bool> {
    let declared = metadata.side.as_deref().unwrap_or("both");
    if !matches!(declared, "client" | "server" | "both" | "common") {
        bail!("invalid metadata side in {metadata_path}: {declared}");
    }
    let folder = if metadata_path.starts_with("mods/client/") {
        "client"
    } else if metadata_path.starts_with("mods/server/") {
        "server"
    } else if metadata_path.starts_with("mods/common/") {
        "both"
    } else {
        declared
    };
    if (folder == "both" && !matches!(declared, "both" | "common"))
        || (folder == "client" && declared == "server")
        || (folder == "server" && declared == "client")
    {
        bail!("conflicting metadata side and directory in {metadata_path}");
    }
    Ok(folder == "both" || folder == "common" || folder == side.as_str())
}

fn destination(metadata_path: &str, filename: &str) -> Result<String> {
    let path = if filename.contains('/') {
        filename.to_owned()
    } else if metadata_path.starts_with("mods/client/")
        || metadata_path.starts_with("mods/server/")
        || metadata_path.starts_with("mods/common/")
    {
        format!("mods/{filename}")
    } else {
        format!(
            "{}/{}",
            metadata_path
                .rsplit_once('/')
                .context("metadata path has no parent")?
                .0,
            filename
        )
    };
    validate_source_paths([path.as_str()])?;
    if !["mods/", "resourcepacks/", "shaderpacks/"]
        .iter()
        .any(|root| path.starts_with(root))
    {
        bail!("metadata destination outside managed roots: {path}");
    }
    Ok(path)
}

fn matches_download(hashes: &ArtifactHashes, download: Option<&Download>) -> Result<bool> {
    let Some(download) = download else {
        return Ok(false);
    };
    let (Some(format), Some(hash)) = (&download.hash_format, &download.hash) else {
        return Ok(false);
    };
    let (actual, length) = match format.as_str() {
        "sha1" => (&hashes.sha1, 40),
        "sha256" => (&hashes.sha256, 64),
        "sha512" => (&hashes.sha512, 128),
        _ => return Ok(false),
    };
    if hash.len() != length || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid metadata download hash");
    }
    Ok(actual.eq_ignore_ascii_case(hash))
}

/// Adopt only installed artifacts whose bytes match A's published metadata.
pub fn adopt_installed(base: &SafeRoot, instance: &SafeRoot, side: Side) -> Result<Adoption> {
    let mut files = Vec::new();
    let mut conflicts = Vec::new();
    let mut destinations = BTreeMap::<String, String>::new();
    let mut candidates = Vec::new();
    for path in base
        .file_paths()?
        .iter()
        .filter(|path| path.ends_with(".pw.toml"))
    {
        let content = base.required_file(path)?;
        let metadata: Metadata = toml::from_str(std::str::from_utf8(&content)?)
            .with_context(|| format!("parsing {path}"))?;
        if !applies(path, &metadata, side)? {
            continue;
        }
        let target = destination(path, &metadata.filename)?;
        if let Some(previous) = destinations.insert(portable_case_key(&target), path.clone()) {
            bail!("metadata destination collision: {previous} and {path}");
        }
        candidates.push((target, metadata));
    }
    let intended_paths: BTreeSet<_> = candidates.iter().map(|(path, _)| path.clone()).collect();
    let case_collisions = instance.case_collisions(&intended_paths)?;
    for (target, metadata) in candidates {
        if case_collisions.contains(&target) {
            conflicts.push(AdoptionConflict {
                path: target,
                reason: "instance_case_collision",
            });
            continue;
        }
        match instance.hash_artifact(&target)? {
            LocalArtifact::Missing => {}
            LocalArtifact::Obstacle => conflicts.push(AdoptionConflict {
                path: target,
                reason: "path_occupied",
            }),
            LocalArtifact::File(hashes) => {
                if matches_download(&hashes, metadata.download.as_ref())? {
                    files.push(ManagedFile {
                        name: metadata.name,
                        path: target,
                        sha256: hashes.sha256,
                    });
                } else {
                    conflicts.push(AdoptionConflict {
                        path: target,
                        reason: "unverified_installed_file",
                    });
                }
            }
        }
    }
    let paths: BTreeSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    validate_source_paths(paths)?;
    Ok(Adoption {
        manifest: Manifest {
            format: "bkmpw:1".into(),
            side,
            files,
        },
        conflicts,
    })
}

pub fn validate_manifest(
    manifest: &Manifest,
    instance: &SafeRoot,
    side: Side,
) -> Result<Vec<AdoptionConflict>> {
    if manifest.format != "bkmpw:1" || manifest.side != side {
        bail!("installed packwiz.json format or side mismatch");
    }
    let unique: BTreeSet<_> = manifest
        .files
        .iter()
        .map(|file| portable_case_key(&file.path))
        .collect();
    if unique.len() != manifest.files.len() {
        bail!("duplicate manifest path");
    }
    validate_source_paths(manifest.files.iter().map(|file| file.path.as_str()))?;
    let paths: BTreeSet<_> = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let case_collisions = instance.case_collisions(&paths)?;
    let mut conflicts = Vec::new();
    for file in &manifest.files {
        if !["mods/", "resourcepacks/", "shaderpacks/"]
            .iter()
            .any(|root| file.path.starts_with(root))
        {
            bail!("manifest path outside managed roots: {}", file.path);
        }
        if file.sha256.len() != 64 || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid manifest SHA-256: {}", file.path);
        }
        if case_collisions.contains(&file.path) {
            conflicts.push(AdoptionConflict {
                path: file.path.clone(),
                reason: "instance_case_collision",
            });
            continue;
        }
        match instance.hash_artifact(&file.path)? {
            LocalArtifact::File(hashes) if hashes.sha256.eq_ignore_ascii_case(&file.sha256) => {}
            _ => conflicts.push(AdoptionConflict {
                path: file.path.clone(),
                reason: "managed_file_changed",
            }),
        }
    }
    Ok(conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn adopts_only_verified_side_files_and_preserves_private_files() {
        let base = TempDir::new().unwrap();
        let instance = TempDir::new().unwrap();
        for root in [base.path(), instance.path()] {
            fs::create_dir_all(root.join("mods/client")).unwrap();
            fs::create_dir_all(root.join("mods/server")).unwrap();
        }
        let hash = format!("{:x}", Sha1::digest(b"known"));
        fs::write(base.path().join("mods/client/good.pw.toml"), format!("name = 'Good'\nfilename = 'good.jar'\n[option]\noptional = true\ndefault = false\n[download]\nhash-format = 'sha1'\nhash = '{hash}'\n")).unwrap();
        fs::write(
            base.path().join("mods/server/server.pw.toml"),
            "name = 'Server'\nfilename = 'server.jar'\n",
        )
        .unwrap();
        fs::write(
            base.path().join("mods/client/bad.pw.toml"),
            "name = 'Bad'\nfilename = 'bad.jar'\n[download]\nhash-format = 'murmur2'\nhash = '1234'\n",
        )
        .unwrap();
        fs::write(instance.path().join("mods/good.jar"), b"known").unwrap();
        fs::write(instance.path().join("mods/bad.jar"), b"unknown").unwrap();
        fs::write(instance.path().join("mods/private.jar"), b"private").unwrap();
        fs::File::create(base.path().join("unrelated-large.bin"))
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        let adoption = adopt_installed(
            &SafeRoot::open(base.path()).unwrap(),
            &SafeRoot::open(instance.path()).unwrap(),
            Side::Client,
        )
        .unwrap();
        assert_eq!(adoption.manifest.files.len(), 1);
        assert_eq!(adoption.manifest.files[0].path, "mods/good.jar");
        assert_eq!(adoption.conflicts.len(), 1);
        assert_eq!(adoption.conflicts[0].path, "mods/bad.jar");
        assert!(
            validate_manifest(
                &adoption.manifest,
                &SafeRoot::open(instance.path()).unwrap(),
                Side::Client
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn case_conflicting_private_file_blocks_adoption() {
        let base = TempDir::new().unwrap();
        let instance = TempDir::new().unwrap();
        fs::create_dir_all(base.path().join("mods/client")).unwrap();
        fs::create_dir_all(instance.path().join("mods")).unwrap();
        let hash = format!("{:x}", Sha1::digest(b"known"));
        fs::write(
            base.path().join("mods/client/good.pw.toml"),
            format!(
                "name='Good'\nfilename='good.jar'\n[download]\nhash-format='sha1'\nhash='{hash}'\n"
            ),
        )
        .unwrap();
        fs::write(instance.path().join("mods/Good.jar"), b"private").unwrap();
        let root = SafeRoot::open(instance.path()).unwrap();
        let adoption =
            adopt_installed(&SafeRoot::open(base.path()).unwrap(), &root, Side::Client).unwrap();
        assert!(adoption.manifest.files.is_empty());
        assert_eq!(adoption.conflicts[0].reason, "instance_case_collision");
    }

    #[test]
    fn invalid_manifest_entry_is_rejected_even_with_case_collision() {
        let instance = TempDir::new().unwrap();
        fs::create_dir_all(instance.path().join("Saves")).unwrap();
        let manifest = Manifest {
            format: "bkmpw:1".into(),
            side: Side::Client,
            files: vec![ManagedFile {
                name: "bad".into(),
                path: "saves/world.dat".into(),
                sha256: "bad".into(),
            }],
        };
        assert!(
            validate_manifest(
                &manifest,
                &SafeRoot::open(instance.path()).unwrap(),
                Side::Client
            )
            .is_err()
        );
    }

    #[test]
    fn common_directory_rejects_client_only_metadata() {
        let base = TempDir::new().unwrap();
        let instance = TempDir::new().unwrap();
        fs::create_dir_all(base.path().join("mods/common")).unwrap();
        fs::write(
            base.path().join("mods/common/contradiction.pw.toml"),
            "name='Wrong'\nfilename='wrong.jar'\nside='client'\n",
        )
        .unwrap();
        assert!(
            adopt_installed(
                &SafeRoot::open(base.path()).unwrap(),
                &SafeRoot::open(instance.path()).unwrap(),
                Side::Server
            )
            .unwrap_err()
            .to_string()
            .contains("conflicting metadata side")
        );
    }
}
