use super::*;
use crate::safe_fs::SafeRoot;
use fs2::FileExt;

pub struct ToolLock(std::fs::File);
impl Drop for ToolLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// The override must identify the same directory in all updater processes.
/// Unix uses /tmp regardless of TMPDIR, with a private, owner-checked directory.
pub fn tool_lock(tool: Tool, exclusive: bool) -> Result<ToolLock> {
    let directory = if let Some(path) = std::env::var_os("MMPU_TOOL_STATE_DIR") {
        PathBuf::from(path)
    } else if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        PathBuf::from(
            std::env::var_os("LOCALAPPDATA")
                .context("set MMPU_TOOL_STATE_DIR to a common private directory")?,
        )
    };
    let root = SafeRoot::open(&directory.canonicalize()?)?;
    #[cfg(test)]
    let identity = format!("test-{}", std::process::id());
    #[cfg(all(not(test), unix))]
    let identity = unsafe { libc::geteuid() }.to_string();
    #[cfg(all(not(test), windows))]
    let identity = "user";
    let relative = format!(".minecraft-modpack-updater-{identity}");
    #[cfg(unix)]
    root.ensure_private_dir(&relative)?;
    #[cfg(windows)]
    root.ensure_dir(&relative)?;
    let private = root.open_dir(&relative)?;
    let file = private.open_lock_file(&format!("{}.lock", tool.name()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
            bail!("untrusted tool lock file permissions");
        }
    }
    if exclusive {
        FileExt::try_lock_exclusive(&file)?;
    } else {
        FileExt::try_lock_shared(&file)?;
    }
    Ok(ToolLock(file))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn permissive_umask_does_not_create_an_unusable_lock() {
        if std::env::var_os("MMPU_LOCK_FIXTURE").is_some() {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            unsafe {
                libc::umask(0o002);
            }
            let guard = tool_lock(Tool::Bkmpw, true).unwrap();
            assert_eq!(guard.0.metadata().unwrap().mode() & 0o777, 0o600);
            guard
                .0
                .set_permissions(std::fs::Permissions::from_mode(0o664))
                .unwrap();
            drop(guard);
            assert!(tool_lock(Tool::Bkmpw, false).is_err());
            return;
        }
        let temp = crate::safe_fs::temp_dir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::locking::tests::permissive_umask_does_not_create_an_unusable_lock",
            ])
            .env("MMPU_LOCK_FIXTURE", "1")
            .env("MMPU_TOOL_STATE_DIR", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    #[test]
    fn private_directory_rejects_shared_permissions_and_locks_exclude_mutation() {
        let temp = crate::safe_fs::temp_dir().unwrap();
        let root = SafeRoot::open(temp.path()).unwrap();
        root.ensure_dir("public").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            temp.path().join("public"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(root.ensure_private_dir("public").is_err());
        root.ensure_private_dir("private").unwrap();
        let one = root.open_lock_file("private/tool.lock").unwrap();
        let two = root.open_lock_file("private/tool.lock").unwrap();
        FileExt::try_lock_shared(&one).unwrap();
        assert!(FileExt::try_lock_exclusive(&two).is_err());
        FileExt::unlock(&one).unwrap();
        FileExt::try_lock_exclusive(&two).unwrap();
        assert!(FileExt::try_lock_shared(&one).is_err());
    }
}
