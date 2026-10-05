use super::{Tool, platform};
use anyhow::{Context, Result, bail};
use std::{
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(super) fn find_program(name: &str) -> Option<PathBuf> {
    find_in(std::env::split_paths(&std::env::var_os("PATH")?), name)
}

fn find_in(dirs: impl Iterator<Item = PathBuf>, name: &str) -> Option<PathBuf> {
    for dir in dirs {
        for suffix in if cfg!(windows) {
            vec![".exe", ".cmd", ""]
        } else {
            vec![""]
        } {
            let path = dir.join(format!("{name}{suffix}"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if path
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 == 0)
                {
                    continue;
                }
            }
            if path.is_file()
                && let Ok(path) = path.canonicalize()
            {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
type Child = std::process::Child;
#[cfg(windows)]
type Child = Box<dyn process_wrap::std::ChildWrapper>;

struct Running(Option<Child>);
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            #[cfg(unix)]
            {
                if let Ok(pid) = i32::try_from(child.id()) {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                let _ = child.kill();
            }
            #[cfg(windows)]
            let _ = child.start_kill();
            let _ = child.wait();
        }
    }
}

pub(super) fn capture(command: Command) -> Result<Vec<u8>> {
    capture_with_timeout(command, Duration::from_secs(15))
}

fn capture_with_timeout(mut command: Command, timeout: Duration) -> Result<Vec<u8>> {
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?);
    #[cfg(unix)]
    let child = {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        command.spawn()?
    };
    #[cfg(windows)]
    let child = {
        use process_wrap::std::{CommandWrap, JobObject};
        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(JobObject).spawn()?
    };
    let mut guard = Running(Some(child));
    let child = guard.0.as_mut().unwrap();
    let deadline = Instant::now() + timeout;
    let status = loop {
        if Instant::now() >= deadline
            || output.metadata()?.len() > 4 * 1024 * 1024
            || errors.metadata()?.len() > 4 * 1024 * 1024
        {
            bail!("tool command exceeded its time or output limit");
        }
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            #[cfg(target_os = "macos")]
            let pid = info.si_pid;
            #[cfg(not(target_os = "macos"))]
            let pid = unsafe { info.si_pid() };
            if pid != 0 {
                // The unreaped group leader holds its PID while descendants are terminated.
                if let Ok(pid) = i32::try_from(child.id()) {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                break child.wait()?;
            }
        }
        #[cfg(windows)]
        if let Some(status) = child.try_wait()? {
            let _ = child.start_kill();
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    guard.0.take();
    if errors.metadata()?.len() > 4 * 1024 * 1024 {
        bail!("tool stderr exceeded its size limit");
    }
    if !status.success() {
        errors.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        errors.take(8192).read_to_string(&mut text)?;
        bail!("tool command failed: {text}");
    }
    output.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    output.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("tool output exceeded its size limit");
    }
    Ok(bytes)
}

pub(super) fn npm(args: &[&str]) -> Result<Vec<u8>> {
    let npm = find_program("npm").context("npm is not available")?;
    #[cfg(not(windows))]
    let mut command = Command::new(npm);
    #[cfg(windows)]
    let mut command = windows_npm(&npm, find_program("node"))?;
    command.args(args);
    let timeout = if args.first() == Some(&"install") {
        180
    } else {
        15
    };
    capture_with_timeout(command, Duration::from_secs(timeout))
}

#[cfg(windows)]
fn windows_npm(npm: &std::path::Path, node: Option<PathBuf>) -> Result<Command> {
    if npm
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Ok(Command::new(npm));
    }
    let script = npm
        .parent()
        .context("npm has no parent")?
        .join("node_modules/npm/bin/npm-cli.js");
    if !script.is_file() {
        bail!("cannot locate the native npm CLI; choose instance download");
    }
    let mut command = Command::new(node.context("node is not available")?);
    command.arg(script);
    Ok(command)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn executable_shims_work_without_an_adjacent_npm_javascript_layout() {
        let dir = tempfile::TempDir::new().unwrap();
        let shim = dir.path().join("npm.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &shim).unwrap();
        let mut command = windows_npm(&shim, None).unwrap();
        command.arg("--list");
        let output = capture(command).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("executable_shims_work")
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn captures_output_and_rejects_failures_and_stalled_children() {
        let mut c = Command::new("sh");
        c.args(["-c", "printf version"]);
        assert_eq!(capture(c).unwrap(), b"version");
        let mut c = Command::new("sh");
        c.args(["-c", "echo failed >&2; exit 1"]);
        assert!(capture(c).unwrap_err().to_string().contains("failed"));
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 60"]);
        let start = Instant::now();
        assert!(capture_with_timeout(c, Duration::from_millis(100)).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn cleans_up_children_when_the_parent_exits_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("leaked");
        let mut command = Command::new("sh");
        command
            .args(["-c", "(sleep 0.2; echo leaked > \"$1\") & exit 0", "sh"])
            .arg(&marker);
        capture(command).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists());
    }
    #[test]
    fn skips_non_executable_path_entries() {
        use std::os::unix::fs::PermissionsExt;
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        std::fs::write(first.path().join("npm"), "ignored").unwrap();
        let executable = second.path().join("npm");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            find_in(
                [first.path().to_owned(), second.path().to_owned()].into_iter(),
                "npm"
            )
            .unwrap(),
            executable.canonicalize().unwrap()
        );
    }
    #[test]
    fn rejects_oversized_stderr_even_when_the_program_exits_successfully() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 4194305 /dev/zero >&2"]);
        assert!(capture(command).is_err());
    }
}

pub(super) fn npm_binary(tool: Tool) -> Result<PathBuf> {
    let root = npm(&["root", "--global"])?;
    let root = PathBuf::from(std::str::from_utf8(&root)?.trim());
    if !root.is_absolute() {
        bail!("npm root is not absolute");
    }
    let suffix = match platform()? {
        "x86_64-unknown-linux-gnu" => "linux-x64",
        "x86_64-pc-windows-msvc" => "win32-x64",
        "x86_64-apple-darwin" => "darwin-x64",
        "aarch64-apple-darwin" => "darwin-arm64",
        _ => unreachable!(),
    };
    let scope = if tool == Tool::Bkmpw {
        "@bro-know-my"
    } else {
        "@jsi-team"
    };
    let base = if tool == Tool::Bkmpw {
        "packwiz"
    } else {
        "minecraft-modpack-updater"
    };
    let package = root.join(tool.package());
    let candidates = [
        package
            .join("node_modules")
            .join(scope)
            .join(format!("{base}-{suffix}"))
            .join(tool.executable()),
        root.join(scope)
            .join(format!("{base}-{suffix}"))
            .join(tool.executable()),
    ];
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("npm native platform package is missing")?
        .canonicalize()
        .context("resolving npm native tool")
}
