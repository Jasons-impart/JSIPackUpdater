use super::*;
use crate::{safe_fs::SafeRoot, transaction};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
struct Request {
    old: Installation,
    new: Installation,
    release: ToolRelease,
    args: Vec<std::ffi::OsString>,
    #[serde(default)]
    terminal: bool,
}

struct Prepared {
    command: Command,
    _guard: ToolLock,
}
static PREPARED: std::sync::OnceLock<std::sync::Mutex<Option<Prepared>>> =
    std::sync::OnceLock::new();

/// Call after the old UI has restored its terminal and closed its window.
pub fn launch_prepared_update() -> Result<()> {
    let Some(mut prepared) = PREPARED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("self-update handoff lock poisoned"))?
        .take()
    else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Keep the shell's foreground job/PID, unmap the old image, close its locks.
        Err(prepared.command.exec().into())
    }
    #[cfg(windows)]
    {
        prepared.command.spawn()?;
        std::mem::forget(prepared._guard); // Retained until the original process exits.
        Ok(())
    }
}

/// Prepare everything while the UI is alive; the helper acquires the retained
/// process lock only after this process exits (also on Windows).
pub fn prepare_self_update(instance: &Path, mut progress: impl FnMut(Progress)) -> Result<bool> {
    let mut old = current_updater()?;
    let instance = instance.canonicalize()?;
    let old_cache = state::cache_path(Tool::Updater, &old.version)?;
    if old.path == instance.join(&old_cache) {
        old.ownership = Ownership::Local;
        old.path = old_cache.into();
    }
    let old_path = old.executable_path(&instance)?;
    progress(Progress::Stage("检查更新器最新版本".into()));
    let release = latest(Tool::Updater)?;
    if !newer(&release.version, &old.version)? {
        progress(Progress::Stage("更新器已是最新版本".into()));
        return Ok(false);
    }
    let new = install_release(&instance, &release, InstallChoice::Local, &mut progress)?;
    let _instance_lock = transaction::lock_instance(&instance)?;
    if transaction::pending(&instance)? {
        bail!("unfinished update exists; rollback before self-update");
    }
    let guard = tool_lock(Tool::Updater, true)?;
    let root = SafeRoot::open(&instance)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory = format!(".jsipack-updater/self-update/{stamp}");
    root.ensure_dir(&directory)?;
    let helper = format!(
        "{directory}/helper{}",
        if cfg!(windows) { ".exe" } else { "" }
    );
    let source = SafeRoot::open(old_path.parent().context("updater has no parent")?)?;
    let name = old_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid updater filename")?;
    if !source.copy_regular_to(name, &root, &helper)? {
        bail!("current updater disappeared");
    }
    if state::hash(&instance.join(&helper))? != old.sha256 {
        bail!("updater changed while preparing helper");
    }
    let request_path = format!("{directory}/request.json");
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let terminal = args.first().is_some_and(|a| a == "tui")
        || args
            .iter()
            .take(2)
            .eq(["tools", "manage"].map(std::ffi::OsString::from).iter());
    if args
        .iter()
        .take(2)
        .eq(["tools", "upgrade"].map(std::ffi::OsString::from).iter())
    {
        args = vec!["--version".into()];
    }
    root.write_atomic(
        &request_path,
        serde_json::to_vec(&Request {
            old,
            new,
            release,
            args,
            terminal,
        })?
        .as_slice(),
    )?;
    progress(Progress::Stage(
        "准备完成，退出后替换并重新启动更新器".into(),
    ));
    let mut command = Command::new(instance.join(helper));
    command
        .args([
            std::ffi::OsStr::new("self-update-helper"),
            instance.as_os_str(),
            std::ffi::OsStr::new(&request_path),
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    *PREPARED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("self-update handoff lock poisoned"))? = Some(Prepared {
        command,
        _guard: guard,
    });
    Ok(true)
}

pub fn finish_self_update(instance: &Path, request_path: &str) -> Result<()> {
    let root = SafeRoot::open(instance)?;
    let request: Request = serde_json::from_slice(&root.required_file(request_path)?)?;
    if request.old.tool != Tool::Updater
        || request.new.tool != Tool::Updater
        || request.release.tool != Tool::Updater
        || request.new.ownership != Ownership::Local
        || normalized_version(&request.new.version)?
            != normalized_version(&request.release.version)?
        || request.new.sha256 != request.release.sha256
    {
        bail!("invalid self-update request");
    }
    let helper = std::env::current_exe()?.canonicalize()?;
    if state::hash(&helper)? != request.old.sha256 {
        bail!("self-update helper does not match the original updater");
    }
    let target = request.old.executable_path(instance)?;
    let new_path = request.new.executable_path(instance)?;
    let backup = format!(
        "{request_path}.old{}",
        if cfg!(windows) { ".exe" } else { "" }
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    let guard = loop {
        match tool_lock(Tool::Updater, true) {
            Ok(guard) => break Ok(guard),
            Err(error) if Instant::now() >= deadline => {
                break Err(
                    error.context("old updater did not exit before the self-update deadline")
                );
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let mut replaced = false;
    let mut npm_changed = false;
    let outcome = (|| -> Result<PathBuf> {
        let _guard = guard.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let _instance_lock = transaction::lock_instance(instance)?;
        if transaction::pending(instance)? {
            bail!("unfinished update appeared before self-update");
        }
        request.release.supported()?;
        acquire::verified(Tool::Updater, &new_path, &request.release)?;
        if state::hash(&target)? != request.old.sha256 {
            bail!("installed updater changed before replacement");
        }
        let destination = SafeRoot::open(target.parent().context("updater has no parent")?)?;
        let target_name = filename(&target)?;
        if request.old.ownership != Ownership::Local
            && !destination.copy_regular_to(target_name, &root, &backup)?
        {
            bail!("cannot back up old updater");
        }
        if request.old.ownership == Ownership::Npm {
            npm_changed = true;
            npm_version(&request.release.version)?;
            acquire::verified(
                Tool::Updater,
                &npm_native_path(Tool::Updater)?,
                &request.release,
            )?;
        } else if request.old.ownership != Ownership::Local {
            let source = SafeRoot::open(new_path.parent().context("new updater has no parent")?)?;
            if !source.copy_regular_to_checked(
                filename(&new_path)?,
                &destination,
                target_name,
                || {
                    if state::hash(&target)? != request.old.sha256 {
                        bail!("updater changed during replacement");
                    }
                    replaced = true;
                    Ok(())
                },
            )? {
                bail!("new updater disappeared");
            }
            acquire::verified(Tool::Updater, &target, &request.release)?;
        }
        let executable = match request.old.ownership {
            Ownership::Npm => npm_native_path(Tool::Updater)?,
            Ownership::Local => new_path.clone(),
            _ => target.clone(),
        };
        let mut restarted = request.new.clone();
        if request.old.ownership != Ownership::Local {
            restarted.path = executable.clone();
            restarted.ownership = request.old.ownership;
        }
        save(instance, &restarted)?;
        Ok(executable)
    })();
    drop(guard); // Release mutation locks before the new program does any work.
    let outcome = outcome.and_then(|executable| {
        let mut command = Command::new(executable);
        command.args(&request.args).env("MMPU_RESTARTED", "1");
        terminal_console(&mut command, request.terminal);
        let mut child = command.spawn()?;
        std::thread::sleep(Duration::from_secs(2));
        if let Some(status) = child.try_wait()?
            && !status.success()
        {
            bail!("new updater failed during startup");
        }
        Ok(child)
    });
    let mut child = match outcome {
        Ok(child) => child,
        Err(mut error) => {
            let recovery_guard = tool_lock(Tool::Updater, true).ok();
            let recovery_instance = transaction::lock_instance(instance).ok();
            let can_mutate = recovery_guard.is_some()
                && recovery_instance.is_some()
                && !transaction::pending(instance).unwrap_or(true);
            let mut recovery = request.old.clone();
            if can_mutate
                && npm_changed
                && state::hash(&target).ok().as_ref() != Some(&request.old.sha256)
            {
                let _ = npm_version(&request.old.version);
                if let Ok(path) = npm_native_path(Tool::Updater)
                    && state::hash(&path).ok().as_ref() == Some(&request.old.sha256)
                {
                    recovery.path = path;
                }
            } else if can_mutate
                && replaced
                && state::hash(&target)
                    .ok()
                    .is_some_and(|h| h.eq_ignore_ascii_case(&request.release.sha256))
            {
                let destination =
                    SafeRoot::open(target.parent().context("updater has no parent")?)?;
                let trusted = SafeRoot::open(helper.parent().context("helper has no parent")?)?;
                if let Err(restore) = trusted.copy_regular_to_checked(
                    filename(&helper)?,
                    &destination,
                    filename(&target)?,
                    || {
                        if !state::hash(&target)?.eq_ignore_ascii_case(&request.release.sha256) {
                            bail!("new updater changed before rollback");
                        }
                        if state::hash(&helper)? != request.old.sha256 {
                            bail!("old updater helper changed before rollback");
                        }
                        Ok(())
                    },
                ) {
                    error = error.context(format!("portable restoration failed: {restore:#}"));
                }
            }
            if state::hash(&recovery.executable_path(instance)?)
                .ok()
                .as_ref()
                != Some(&request.old.sha256)
            {
                // The helper is a verified old executable: recovery never requires the registry.
                let relative = state::cache_path(Tool::Updater, &request.old.version)?;
                let cached = instance.join(&relative);
                if can_mutate && !cached.exists() {
                    let _ = SafeRoot::open(helper.parent().context("helper has no parent")?)?
                        .copy_regular_to(filename(&helper)?, &root, &relative);
                }
                if state::hash(&cached).ok().as_ref() == Some(&request.old.sha256) {
                    recovery.path = relative.into();
                    recovery.ownership = Ownership::Local;
                } else {
                    recovery.path = helper;
                    recovery.ownership = Ownership::Portable;
                }
            }
            let executable = recovery.executable_path(instance)?;
            if state::hash(&executable)? != request.old.sha256 {
                bail!("old updater recovery copy changed: {error:#}");
            }
            if can_mutate {
                let _ = save(instance, &recovery);
            }
            let _ = root.write_atomic(
                &format!("{request_path}.error.txt"),
                format!("{error:#}").as_bytes(),
            );
            let mut restart = Command::new(executable);
            restart
                .args(request.args)
                .env("MMPU_SELF_UPDATE_ERROR", format!("{error:#}"));
            if recovery.ownership == Ownership::Npm {
                restart.env("MMPU_MANAGED_BY_NPM", "1");
            } else {
                restart.env_remove("MMPU_MANAGED_BY_NPM");
            }
            terminal_console(&mut restart, request.terminal);
            drop(recovery_instance);
            drop(recovery_guard);
            let mut old = restart.spawn()?;
            if request.terminal {
                let _ = old.wait();
            }
            return Err(error);
        }
    };
    let bookkeeping = root.write_atomic(&format!("{request_path}.complete"), &b"ok"[..]);
    if request.terminal {
        wait_terminal(&mut child, bookkeeping)?;
    } else {
        bookkeeping?;
    }
    Ok(())
}

fn wait_terminal(child: &mut std::process::Child, bookkeeping: Result<()>) -> Result<()> {
    let status = child.wait();
    bookkeeping?;
    let status = status?;
    if !status.success() {
        bail!("restarted updater exited with {status}");
    }
    Ok(())
}

fn terminal_console(command: &mut Command, terminal: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if terminal {
            command.creation_flags(0x10); /* CREATE_NEW_CONSOLE */
        }
    }
    #[cfg(unix)]
    {
        let _ = (command, terminal);
    }
}

fn filename(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|n| n.to_str())
        .context("invalid executable filename")
}
fn npm_version(version: &str) -> Result<()> {
    let spec = format!(
        "{}@{}",
        Tool::Updater.package(),
        normalized_version(version)?
    );
    process::npm_progress(
        &["install", "--global", "--no-audit", "--no-fund", &spec],
        &mut |s| eprint!("{s}"),
    )?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn terminal_wait_preserves_failure_and_outlives_failed_bookkeeping() {
        for bad_marker in [false, true] {
            let temp = tempfile::NamedTempFile::new().unwrap();
            let mut child = Command::new("sh")
                .args(["-c", "sleep 2.1; printf done > \"$1\"; exit 7", "sh"])
                .arg(temp.path())
                .spawn()
                .unwrap();
            let marker = if bad_marker {
                Err(anyhow::anyhow!("completion marker failed"))
            } else {
                Ok(())
            };
            assert!(wait_terminal(&mut child, marker).is_err());
            assert_eq!(std::fs::read(temp.path()).unwrap(), b"done");
        }
    }
    #[test]
    fn portable_replacement_preserves_backup_and_rolls_back_startup_failure() {
        for fail in [false, true] {
            let temp = tempfile::TempDir::new().unwrap();
            let instance = temp.path().canonicalize().unwrap();
            let root = SafeRoot::open(&instance).unwrap();
            let original = std::env::current_exe().unwrap().canonicalize().unwrap();
            let target = instance.join("updater");
            std::fs::copy(&original, &target).unwrap();
            let old = Installation {
                tool: Tool::Updater,
                version: env!("CARGO_PKG_VERSION").into(),
                ownership: Ownership::Portable,
                sha256: state::hash(&target).unwrap(),
                path: target.clone(),
            };
            let relative = state::cache_path(Tool::Updater, "0.1.1").unwrap();
            let lock_probe = if cfg!(target_os = "linux") {
                "flock -n -s \"$2\" -c true || exit 2\n"
            } else {
                ""
            };
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'minecraft-modpack-updater 0.1.1'; exit 0; fi\nexit {}\n",
                if fail { 1 } else { 0 }
            );
            let script = script.replace("\nexit ", &format!("\n{lock_probe}exit "));
            root.write_atomic(&relative, script.as_bytes()).unwrap();
            root.set_unix_mode(&relative, 0o700).unwrap();
            let sha256 = state::hash(&instance.join(&relative)).unwrap();
            let checksum = if fail {
                sha256.to_uppercase()
            } else {
                sha256.clone()
            };
            let new = Installation {
                tool: Tool::Updater,
                version: "0.1.1".into(),
                ownership: Ownership::Local,
                path: relative.into(),
                sha256: checksum.clone(),
            };
            let release = ToolRelease {
                tool: Tool::Updater,
                version: "0.1.1".into(),
                platform: platform().unwrap().into(),
                url: format!(
                    "https://github.com/{}/releases/download/v0.1.1/minecraft-modpack-updater-v0.1.1-{}",
                    Tool::Updater.repository(),
                    platform().unwrap()
                ),
                sha256: checksum,
                minimum_updater: None,
            };
            let request = ".jsipack-updater/self-update/test/request.json";
            root.write_atomic(
                request,
                serde_json::to_vec(&Request {
                    old: old.clone(),
                    new,
                    release,
                    args: vec![
                        "--list".into(),
                        PathBuf::from(
                            std::env::var_os("MMPU_TOOL_STATE_DIR")
                                .unwrap_or_else(|| "/tmp".into()),
                        )
                        .join(format!(
                            ".minecraft-modpack-updater-test-{}/minecraft-modpack-updater.lock",
                            std::process::id()
                        ))
                        .into_os_string(),
                    ],
                    terminal: false,
                })
                .unwrap()
                .as_slice(),
            )
            .unwrap();
            let result = finish_self_update(&instance, request);
            assert_eq!(result.is_err(), fail);
            assert_eq!(
                state::hash(&target).unwrap(),
                if fail { old.sha256.clone() } else { sha256 }
            );
            assert_eq!(
                state::hash(&instance.join(format!("{request}.old"))).unwrap(),
                old.sha256
            );
            if fail {
                assert!(
                    !root
                        .required_file(&format!("{request}.error.txt"))
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert_eq!(
                    root.required_file(&format!("{request}.complete")).unwrap(),
                    b"ok"
                );
            }
        }
    }
}
