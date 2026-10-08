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

fn capture_with_timeout(command: Command, timeout: Duration) -> Result<Vec<u8>> {
    capture_progress(command, timeout, &mut |_| {})
}

fn capture_progress(
    mut command: Command,
    timeout: Duration,
    progress: &mut impl FnMut(String),
) -> Result<Vec<u8>> {
    let stdout_file = tempfile::NamedTempFile::new()?;
    let stderr_file = tempfile::NamedTempFile::new()?;
    let mut output = stdout_file.reopen()?;
    let mut errors = stderr_file.reopen()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout_file.reopen()?)
        .stderr(stderr_file.reopen()?);
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
    let mut streams = [OutputStream::default(), OutputStream::default()];
    let status = loop {
        if Instant::now() >= deadline
            || output.metadata()?.len() > 4 * 1024 * 1024
            || errors.metadata()?.len() > 4 * 1024 * 1024
        {
            bail!("tool command exceeded its time or output limit");
        }
        for (file, stream) in [&mut output, &mut errors].into_iter().zip(&mut streams) {
            stream.drain(file, false, progress)?;
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
    for (file, stream) in [&mut output, &mut errors].into_iter().zip(&mut streams) {
        stream.drain(file, true, progress)?;
    }
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
    npm_progress(args, &mut |_| {})
}

pub(super) fn npm_progress(args: &[&str], progress: &mut impl FnMut(String)) -> Result<Vec<u8>> {
    let npm = find_program("npm").context("npm is not available")?;
    #[cfg(not(windows))]
    let mut command = Command::new(npm);
    #[cfg(windows)]
    let mut command = windows_npm(&npm, find_program("node"))?;
    command.args(args);
    let timeout = if args.first() == Some(&"install") {
        600
    } else {
        15
    };
    capture_progress(command, Duration::from_secs(timeout), progress)
}

#[derive(Default)]
struct OutputStream {
    position: u64,
    pending: Vec<u8>,
}
impl OutputStream {
    fn drain(
        &mut self,
        file: &mut std::fs::File,
        eof: bool,
        progress: &mut impl FnMut(String),
    ) -> Result<()> {
        let length = file.metadata()?.len();
        if length > 4 * 1024 * 1024 {
            bail!("tool output exceeded its size limit");
        }
        file.seek(SeekFrom::Start(self.position))?;
        self.position += Read::by_ref(file)
            .take(length.saturating_sub(self.position))
            .read_to_end(&mut self.pending)? as u64;
        let mut cursor = 0;
        while cursor < self.pending.len() {
            match std::str::from_utf8(&self.pending[cursor..]) {
                Ok(_) => {
                    cursor = self.pending.len();
                    break;
                }
                Err(error) => {
                    cursor += error.valid_up_to();
                    if let Some(length) = error.error_len() {
                        cursor += length;
                    } else {
                        if eof {
                            cursor = self.pending.len();
                        }
                        break;
                    }
                }
            }
        }
        if cursor > 0 {
            progress(String::from_utf8_lossy(&self.pending[..cursor]).into_owned());
            self.pending.drain(..cursor);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn windows_npm(npm: &std::path::Path, node: Option<PathBuf>) -> Result<Command> {
    if npm
        .extension()
        .and_then(|e| e.to_str())
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
        let dir = crate::safe_fs::temp_dir().unwrap();
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
        let dir = crate::safe_fs::temp_dir().unwrap();
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
        let first = crate::safe_fs::temp_dir().unwrap();
        let second = crate::safe_fs::temp_dir().unwrap();
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
    #[test]
    fn progress_preserves_split_utf8_and_final_output() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut reader = temp.reopen().unwrap();
        let mut stream = OutputStream::default();
        let mut chunks = Vec::new();
        std::fs::write(temp.path(), [0xe4, 0xb8]).unwrap();
        stream
            .drain(&mut reader, false, &mut |s| chunks.push(s))
            .unwrap();
        assert!(chunks.is_empty());
        std::fs::write(temp.path(), [0xe4, 0xb8, 0xad]).unwrap();
        stream
            .drain(&mut reader, true, &mut |s| chunks.push(s))
            .unwrap();
        assert_eq!(chunks.concat(), "中");
        chunks.clear();
        std::fs::write(temp.path(), vec![0xff; 1024 * 1024]).unwrap();
        stream = OutputStream::default();
        stream
            .drain(&mut reader, true, &mut |s| chunks.push(s))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chars().count(), 1024 * 1024);
        chunks.clear();
        let mut command = Command::new("sh");
        command.args(["-c", "printf last-output; printf last-error >&2"]);
        capture_progress(command, Duration::from_secs(2), &mut |s| chunks.push(s)).unwrap();
        let all = chunks.concat();
        assert!(all.contains("last-output") && all.contains("last-error"));
    }
}

pub(super) fn npm_binary(tool: Tool) -> Result<PathBuf> {
    let root = npm_global_root()?;
    npm_binary_in(tool, &root)
}

pub(super) fn npm_global_root() -> Result<PathBuf> {
    let root = npm(&["root", "--global"])?;
    let root = PathBuf::from(std::str::from_utf8(&root)?.trim());
    if !root.is_absolute() {
        bail!("npm root is not absolute");
    }
    Ok(root)
}

fn npm_binary_in(tool: Tool, root: &std::path::Path) -> Result<PathBuf> {
    let suffix = match platform()? {
        "x86_64-unknown-linux-gnu" => "linux-x64",
        "aarch64-unknown-linux-gnu" => "linux-arm64",
        "x86_64-pc-windows-msvc" => "win32-x64",
        "aarch64-pc-windows-msvc" => "win32-arm64",
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

#[cfg(test)]
#[test]
fn npm_discovery_selects_the_host_native_file_for_both_tools() {
    let root = crate::safe_fs::temp_dir().unwrap();
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    for tool in [Tool::Bkmpw, Tool::Updater] {
        let native = root.path().join(format!("{}-{os}-{arch}", tool.package()));
        std::fs::create_dir_all(&native).unwrap();
        let file = native.join(tool.executable());
        std::fs::write(&file, b"native file fixture").unwrap();
        assert_eq!(
            npm_binary_in(tool, root.path()).unwrap(),
            file.canonicalize().unwrap()
        );
    }
}
