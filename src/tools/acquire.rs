use super::progress::transfer;
use super::*;
use crate::{safe_fs::SafeRoot, transaction};
use std::time::Duration;

#[cfg(all(test, target_os = "linux"))]
#[path = "acquire_tests.rs"]
mod npm_tests;

pub(crate) fn verified(tool: Tool, path: &Path, release: &ToolRelease) -> Result<()> {
    if !state::hash(path)?.eq_ignore_ascii_case(&release.sha256) {
        bail!("downloaded tool SHA-256 does not match the release");
    }
    let version = match tool {
        Tool::Bkmpw => crate::bkmpw::installed_version(path)?,
        Tool::Updater => inspect_updater(path)?,
    };
    if normalized_version(&version)? != normalized_version(&release.version)? {
        bail!("tool executable reports a different version than its release");
    }
    Ok(())
}

fn local(
    instance: &Path,
    release: &ToolRelease,
    progress: &mut impl FnMut(Progress),
) -> Result<Installation> {
    let relative = state::cache_path(release.tool, &release.version)?;
    let root = SafeRoot::open(instance)?;
    let path = instance.join(&relative);
    if let crate::safe_fs::LocalArtifact::File(existing) = root.hash_artifact(&relative)?
        && !existing.sha256.eq_ignore_ascii_case(&release.sha256)
    {
        bail!("version cache contains different content; refusing to replace an existing tool");
    }
    if verified(release.tool, &path, release).is_ok() {
        progress(Progress::Stage("复用已验证的工具缓存".into()));
    } else {
        root.ensure_dir(".jsipack-updater/downloads")?;
        let temp = tempfile::Builder::new()
            .prefix("tool-")
            .tempdir_in(instance.join(".jsipack-updater/downloads"))?;
        let staging = temp.path().canonicalize()?;
        let destination = staging.join(release.tool.executable());
        progress(Progress::Stage(format!(
            "下载 {} {}",
            release.tool.name(),
            release.version
        )));
        let response = releases::client()?
            .get(&release.url)
            .timeout(Duration::from_secs(600))
            .send()?
            .error_for_status()?;
        let total = response.content_length();
        let file = std::fs::File::create(&destination)?;
        transfer(response, &file, total, progress)?;
        file.sync_all()?;
        drop(file);
        let staged = SafeRoot::open(&staging)?;
        #[cfg(unix)]
        staged.set_unix_mode(&release.tool.executable(), 0o700)?;
        progress(Progress::Stage("校验 SHA-256、工具协议和版本".into()));
        verified(release.tool, &destination, release)?;
        progress(Progress::Stage("安装到实例目录；保留旧版本".into()));
        if !staged.copy_regular_to(&release.tool.executable(), &root, &relative)? {
            bail!("staged tool disappeared");
        }
        verified(release.tool, &path, release)?;
    }
    Ok(Installation {
        tool: release.tool,
        version: normalized_version(&release.version)?,
        ownership: Ownership::Local,
        path: relative.into(),
        sha256: release.sha256.clone(),
    })
}

pub fn install(
    instance: &Path,
    tool: Tool,
    choice: InstallChoice,
    mut progress: impl FnMut(Progress),
) -> Result<Installation> {
    progress(Progress::Stage(format!("检查 {} 最新版本", tool.name())));
    let release = latest(tool)?;
    install_release(instance, &release, choice, &mut progress)
}

pub fn install_release(
    instance: &Path,
    release: &ToolRelease,
    choice: InstallChoice,
    mut progress: impl FnMut(Progress),
) -> Result<Installation> {
    release.supported()?;
    let _instance_lock = transaction::lock_instance(instance)?;
    if transaction::pending(instance)? {
        bail!("unfinished update exists; rollback before changing tools");
    }
    let _tool_lock =
        tool_lock(release.tool, true).context("another operation is using this tool")?;
    install_locked(instance, release, choice, &mut progress)
}

fn install_locked(
    instance: &Path,
    release: &ToolRelease,
    choice: InstallChoice,
    mut progress: impl FnMut(Progress),
) -> Result<Installation> {
    let installed = match choice {
        InstallChoice::Local => local(instance, release, &mut progress)?,
        InstallChoice::Npm => {
            let previous = if release.tool == Tool::Bkmpw {
                state::discover_unlocked(instance)?
            } else {
                None
            };
            let backup = npm_backup(instance, release.tool)?;
            if let Some(backup) = &backup
                && previous
                    .as_ref()
                    .is_none_or(|i| i.ownership == Ownership::Npm)
            {
                // Make recovery durable before npm may remove the old global package.
                save(instance, backup)?;
            }
            let result = (|| -> Result<Installation> {
                progress(Progress::Stage(format!(
                    "npm 全局安装 {} {}",
                    release.tool.package(),
                    release.version
                )));
                let spec = format!(
                    "{}@{}",
                    release.tool.package(),
                    normalized_version(&release.version)?
                );
                process::npm_progress(
                    &["install", "--global", "--no-audit", "--no-fund", &spec],
                    &mut |s| progress(Progress::Output(s)),
                )?;
                let path = npm_native_path(release.tool)?;
                progress(Progress::Stage("验证 npm 安装的原生工具".into()));
                verified(release.tool, &path, release)?;
                let installed = Installation {
                    tool: release.tool,
                    version: normalized_version(&release.version)?,
                    ownership: Ownership::Npm,
                    path,
                    sha256: release.sha256.clone(),
                };
                save(instance, &installed)?;
                Ok(installed)
            })();
            match result {
                Ok(installed) => installed,
                Err(error) => {
                    if let Some(backup) = backup {
                        let spec = format!("{}@{}", release.tool.package(), backup.version);
                        progress(Progress::Stage("npm 安装失败，正在恢复旧版全局安装".into()));
                        let restored = process::npm_progress(
                            &["install", "--global", "--no-audit", "--no-fund", &spec],
                            &mut |s| progress(Progress::Output(s)),
                        )
                        .is_ok()
                            && npm_native_path(release.tool)
                                .ok()
                                .and_then(|p| state::hash(&p).ok())
                                .as_ref()
                                == Some(&backup.sha256);
                        let recovery = previous
                            .as_ref()
                            .filter(|i| i.ownership != Ownership::Npm || restored)
                            .unwrap_or(&backup);
                        save(instance, recovery)
                            .context("npm failed; could not activate the recovery copy")?;
                        progress(Progress::Stage(
                            if restored {
                                "已恢复旧版 npm 工具"
                            } else {
                                "已切回实例中的旧版工具备份；npm 全局安装恢复失败"
                            }
                            .into(),
                        ));
                    } else {
                        if let Err(cleanup) = process::npm_progress(
                            &["uninstall", "--global", release.tool.package()],
                            &mut |s| progress(Progress::Output(s)),
                        ) {
                            return Err(error).context(format!("npm cleanup failed; repair the global installation before reuse: {cleanup:#}"));
                        }
                    }
                    return Err(error);
                }
            }
        }
    };
    // Activation is last: a failed download/probe never switches the recorded executable.
    if choice == InstallChoice::Local {
        save(instance, &installed)?;
    }
    progress(Progress::Stage(format!(
        "{} {} 已就绪",
        release.tool.name(),
        installed.version
    )));
    Ok(installed)
}

fn npm_backup(instance: &Path, tool: Tool) -> Result<Option<Installation>> {
    let global = process::npm_global_root()?;
    match std::fs::symlink_metadata(&global) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let root = SafeRoot::open(&global.canonicalize()?)?;
    match root.read(&format!("{}/package.json", tool.package()))? {
        crate::safe_fs::LocalFile::Missing => return Ok(None),
        crate::safe_fs::LocalFile::Obstacle => bail!("npm package metadata is not a regular file"),
        crate::safe_fs::LocalFile::File(_) => {}
    }
    let path =
        npm_native_path(tool).context("cannot safely back up the existing npm installation")?;
    let version = normalized_version(&match tool {
        Tool::Bkmpw => crate::bkmpw::installed_version(&path)?,
        Tool::Updater => inspect_updater(&path)?,
    })?;
    let sha256 = state::hash(&path)?;
    let relative = state::cache_path(tool, &version)?;
    let source = SafeRoot::open(path.parent().context("npm binary has no parent")?)?;
    let root = SafeRoot::open(instance)?;
    if let Ok(existing) = state::hash(&instance.join(&relative))
        && existing != sha256
    {
        bail!("recovery cache already contains a different binary of this version");
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid npm binary filename")?;
    if !source.copy_regular_to(name, &root, &relative)? {
        bail!("npm recovery copy disappeared");
    }
    if state::hash(&instance.join(&relative))? != sha256 {
        bail!("npm binary changed during backup");
    }
    Ok(Some(Installation {
        tool,
        version,
        ownership: Ownership::Local,
        path: relative.into(),
        sha256,
    }))
}

pub fn upgrade_bkmpw(instance: &Path, progress: impl FnMut(Progress)) -> Result<Installation> {
    let release = latest(Tool::Bkmpw)?;
    release.supported()?;
    let _instance_lock = transaction::lock_instance(instance)?;
    if transaction::pending(instance)? {
        bail!("unfinished update exists; rollback before changing tools");
    }
    let _tool_lock = tool_lock(Tool::Bkmpw, true)?;
    let installed = state::discover_unlocked(instance)?
        .context("bkmpw is missing; choose an installation method first")?;
    let choice = match installed.ownership {
        Ownership::Local => InstallChoice::Local,
        Ownership::Npm => InstallChoice::Npm,
        _ => bail!(
            "external bkmpw is managed by another installer; choose a new local or npm installation explicitly"
        ),
    };
    if !newer(&release.version, &installed.version)? {
        return Ok(installed);
    }
    install_locked(instance, &release, choice, progress)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn verified_cache_is_reused_and_wrong_hash_or_version_is_rejected() {
        let instance = crate::safe_fs::temp_dir().unwrap();
        let relative = state::cache_path(Tool::Bkmpw, "0.2.5").unwrap();
        let root = SafeRoot::open(instance.path()).unwrap();
        let script = b"#!/bin/sh\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true,\"data\":{\"protocolVersion\":1,\"cliVersion\":\"0.2.5\"}}'\n";
        root.write_atomic(&relative, &script[..]).unwrap();
        root.set_unix_mode(&relative, 0o700).unwrap();
        let path = instance.path().join(&relative);
        let mut release = ToolRelease {
            tool: Tool::Bkmpw,
            version: "0.2.5".into(),
            platform: platform().unwrap().into(),
            url: format!(
                "https://github.com/{}/releases/download/v0.2.5/bkmpw-v0.2.5-{}",
                Tool::Bkmpw.repository(),
                platform().unwrap()
            ),
            sha256: state::hash(&path).unwrap(),
            minimum_updater: None,
        };
        let mut events = Vec::new();
        let installed = install_release(instance.path(), &release, InstallChoice::Local, |p| {
            events.push(p)
        })
        .unwrap();
        assert!(
            events
                .iter()
                .all(|p| !matches!(p, Progress::Download { .. }))
        );
        assert_eq!(
            discover(instance.path()).unwrap().unwrap().path,
            installed.path
        );
        release.sha256 = "0".repeat(64);
        assert!(verified(Tool::Bkmpw, &path, &release).is_err());
        assert!(install_release(instance.path(), &release, InstallChoice::Local, |_| {}).is_err());
        assert_eq!(state::hash(&path).unwrap(), installed.sha256);
        assert_eq!(
            state::discover_unlocked(instance.path())
                .unwrap()
                .unwrap()
                .sha256,
            installed.sha256
        );
        release.sha256 = installed.sha256;
        release.version = "0.2.6".into();
        assert!(verified(Tool::Bkmpw, &path, &release).is_err());
        assert_eq!(discover(instance.path()).unwrap().unwrap().version, "0.2.5");
        release.version = "0.2.5".into();
        let guard = transaction::lock_instance(instance.path()).unwrap();
        assert!(install_release(instance.path(), &release, InstallChoice::Local, |_| {}).is_err());
        drop(guard);
        root.write_atomic(".jsipack-updater/active.json", &b"{}"[..])
            .unwrap();
        assert!(install_release(instance.path(), &release, InstallChoice::Local, |_| {}).is_err());
    }
}
