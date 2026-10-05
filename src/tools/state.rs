use super::*;
use crate::{
    bkmpw,
    safe_fs::{LocalFile, SafeRoot},
};
use std::path::Path;

pub(crate) fn cache_path(tool: Tool, version: &str) -> Result<String> {
    Ok(format!(
        ".jsipack-updater/bin/{}/{}/{}/{}",
        tool.name(),
        normalized_version(version)?,
        platform()?,
        tool.executable()
    ))
}

fn record_path(tool: Tool) -> String {
    format!(".jsipack-updater/tools/{}.json", tool.name())
}

impl Installation {
    pub fn executable_path(&self, instance: &Path) -> Result<PathBuf> {
        if self.ownership == Ownership::Local {
            let expected = cache_path(self.tool, &self.version)?;
            if self.path != Path::new(&expected) {
                bail!("invalid instance tool location");
            }
            Ok(instance.join(expected))
        } else {
            if !self.path.is_absolute() {
                bail!("external tool location must be absolute");
            }
            Ok(self.path.clone())
        }
    }
}

pub fn save(instance: &Path, installed: &Installation) -> Result<()> {
    let root = SafeRoot::open(instance)?;
    root.write_atomic(
        &record_path(installed.tool),
        serde_json::to_vec_pretty(installed)?.as_slice(),
    )
}

fn load(instance: &Path, tool: Tool) -> Result<Option<Installation>> {
    match SafeRoot::open(instance)?.read(&record_path(tool))? {
        LocalFile::Missing => Ok(None),
        LocalFile::Obstacle => bail!("tool installation record is not a regular file"),
        LocalFile::File(bytes) => {
            let installed: Installation = serde_json::from_slice(&bytes)?;
            if installed.tool != tool {
                bail!("tool installation record has the wrong tool identity");
            }
            normalized_version(&installed.version)?;
            Ok(Some(installed))
        }
    }
}

pub(crate) fn hash(path: &Path) -> Result<String> {
    let root = SafeRoot::open(path.parent().context("tool has no parent directory")?)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid tool filename")?;
    use sha2::Digest;
    use std::io::Read;
    let mut file = root.open_regular(name)?.context("tool is missing")?;
    let mut digest = sha2::Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        bytes += count as u64;
        if bytes > 256 * 1024 * 1024 {
            bail!("tool exceeds 256 MiB");
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn recorded(instance: &Path) -> Result<Option<Installation>> {
    if let Some(installed) = load(instance, Tool::Bkmpw)? {
        let path = installed.executable_path(instance)?;
        if let Ok(actual) = hash(&path)
            && actual.eq_ignore_ascii_case(&installed.sha256)
            && bkmpw::installed_version(&path)
                .ok()
                .and_then(|v| normalized_version(&v).ok())
                == Some(normalized_version(&installed.version)?)
        {
            return Ok(Some(installed));
        }
    }
    Ok(None)
}

fn candidate(path: PathBuf, ownership: Ownership) -> Option<Installation> {
    Some(Installation {
        tool: Tool::Bkmpw,
        version: normalized_version(&bkmpw::installed_version(&path).ok()?).ok()?,
        ownership,
        sha256: hash(&path).ok()?,
        path,
    })
}

pub fn discover(instance: &Path) -> Result<Option<Installation>> {
    let _guard = tool_lock(Tool::Bkmpw, false).context("another operation is changing bkmpw")?;
    discover_unlocked(instance)
}

pub(super) fn discover_unlocked(instance: &Path) -> Result<Option<Installation>> {
    if let Some(installed) = recorded(instance)? {
        return Ok(Some(installed));
    }
    // npm's platform package contains the native file; shims are never handed to the Rust runner.
    if let Ok(path) = process::npm_binary(Tool::Bkmpw)
        && let Some(installed) = candidate(path, Ownership::Npm)
    {
        return Ok(Some(installed));
    }
    if let Some(path) = process::find_program("bkmpw")
        && path.extension().and_then(|e| e.to_str()).is_none_or(|e| {
            !["cmd", "bat", "js"]
                .iter()
                .any(|suffix| e.eq_ignore_ascii_case(suffix))
        })
        && let Some(installed) = candidate(path, Ownership::External)
    {
        return Ok(Some(installed));
    }
    Ok(None)
}

pub fn current_updater() -> Result<Installation> {
    let path = std::env::current_exe()?.canonicalize()?;
    Ok(Installation {
        tool: Tool::Updater,
        version: env!("CARGO_PKG_VERSION").into(),
        ownership: if std::env::var_os("MMPU_MANAGED_BY_NPM").is_some() {
            Ownership::Npm
        } else {
            Ownership::Portable
        },
        sha256: hash(&path)?,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_are_ordered_numerically_and_cannot_escape_the_cache() {
        assert!(newer("v0.10.0", "0.2.5").unwrap());
        assert!(!newer("0.2.5", "0.2.5").unwrap());
        for bad in [
            "../../x",
            "1.2.3;echo",
            "1.2.3-beta",
            "1.2",
            "1.2.999999999",
        ] {
            assert!(normalized_version(bad).is_err());
        }
    }
    #[test]
    fn local_records_cannot_execute_an_arbitrary_path() {
        let root = tempfile::TempDir::new().unwrap();
        let installed = Installation {
            tool: Tool::Bkmpw,
            version: "0.2.5".into(),
            ownership: Ownership::Local,
            path: "../other".into(),
            sha256: "a".repeat(64),
        };
        save(root.path(), &installed).unwrap();
        assert!(
            load(root.path(), Tool::Bkmpw)
                .unwrap()
                .unwrap()
                .executable_path(root.path())
                .is_err()
        );
    }
    #[cfg(unix)]
    fn fake(root: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(format!("bkmpw-{version}"));
        let output = format!(
            "{{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}}\n{{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true,\"data\":{{\"protocolVersion\":1,\"cliVersion\":\"{version}\"}}}}"
        );
        std::fs::write(&path, format!("#!/bin/sh\ncat <<'EOF'\n{output}\nEOF\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    #[cfg(unix)]
    fn prefixed_versions_keep_recorded_ownership_and_bad_candidates_fall_through() {
        let root = tempfile::TempDir::new().unwrap();
        let path = fake(root.path(), "v0.2.5");
        let installed = candidate(path.clone(), Ownership::Npm).unwrap();
        save(root.path(), &installed).unwrap();
        assert_eq!(
            recorded(root.path()).unwrap().unwrap().ownership,
            Ownership::Npm
        );
        let prerelease = fake(root.path(), "0.2.5-beta");
        let found = [(prerelease, Ownership::Npm), (path, Ownership::External)]
            .into_iter()
            .find_map(|(p, o)| candidate(p, o))
            .unwrap();
        assert_eq!(found.ownership, Ownership::External);
    }
}
