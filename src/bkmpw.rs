use crate::managed::Side;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::{ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

const MAX_EVENT_LINE: u64 = 64 * 1024;

pub const PINNED_VERSION: &str = "v0.2.5";

pub fn verify_pinned_binary(executable: &Path) -> Result<()> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const EXPECTED: &str = "dce331c2b86ac701dc2d4a38b16fae0512ed0fdda9d68d0547a286683db9ab7c";
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    const EXPECTED: &str = "c27f51b757269e70fd2eb93840f6c170eae957e250792165ce0263178209e156";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    const EXPECTED: &str = "a51e9ac418f9a2ad287565ba2b591e488c600c848b5b1b2d90c5b54d1cf0e943";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    const EXPECTED: &str = "72e1a25fef033604ff5d59146a8158a311425b92ecbf5b55035e6bb238a3936d";
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    const EXPECTED: &str = "";
    if EXPECTED.is_empty() {
        bail!("no pinned bkmpw binary is published for this platform");
    }
    let mut file = std::fs::File::open(executable)?;
    let mut hasher = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hasher.update(&bytes[..count]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != EXPECTED {
        bail!("bundled bkmpw checksum does not match pinned {PINNED_VERSION} binary");
    }
    Ok(())
}

struct BkmPwChild {
    #[cfg(unix)]
    inner: std::process::Child,
    #[cfg(windows)]
    inner: Box<dyn process_wrap::std::ChildWrapper>,
}

impl BkmPwChild {
    fn spawn(command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let mut command = command;
            command.process_group(0);
            Ok(Self {
                inner: command.spawn()?,
            })
        }
        #[cfg(windows)]
        {
            use process_wrap::std::{CommandWrap, JobObject};
            let mut wrapped = CommandWrap::from(command);
            wrapped.wrap(JobObject);
            Ok(Self {
                inner: wrapped.spawn()?,
            })
        }
    }

    fn stdout(&mut self) -> Option<ChildStdout> {
        #[cfg(unix)]
        {
            self.inner.stdout.take()
        }
        #[cfg(windows)]
        {
            self.inner.stdout().take()
        }
    }

    fn has_exited_without_reap(&mut self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.inner.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "macos")]
            let pid = info.si_pid;
            #[cfg(not(target_os = "macos"))]
            let pid = unsafe { info.si_pid() };
            Ok(pid != 0)
        }
        #[cfg(windows)]
        {
            Ok(self.inner.try_wait()?.is_some())
        }
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait()
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        {
            if let Ok(pid) = i32::try_from(self.inner.id()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = self.inner.kill();
        }
        #[cfg(windows)]
        {
            let _ = self.inner.start_kill();
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub protocol_version: u64,
    pub command: String,
    pub sequence: u64,
    pub event: String,
    pub phase: Option<String>,
    pub message: Option<String>,
    pub ok: Option<bool>,
    pub data: Option<Value>,
    pub error: Option<MachineError>,
}

#[derive(Debug, Deserialize)]
pub struct MachineError {
    pub code: String,
    pub message: String,
}

pub fn verify_protocol(executable: &Path) -> Result<()> {
    let event = run(executable, "protocol-version", &[], |_| {})?;
    let version = event
        .data
        .as_ref()
        .and_then(|data| data.get("protocolVersion"))
        .and_then(Value::as_u64);
    if version != Some(1) {
        bail!("bkmpw machine protocol version is not 1");
    }
    Ok(())
}

pub fn prepare_pack(executable: &Path, source: &Path, progress: impl FnMut(&Event)) -> Result<()> {
    run(executable, "prepare-pack", &[source.as_os_str()], progress)?;
    Ok(())
}

pub fn sync(
    executable: &Path,
    source: &Path,
    target: &Path,
    side: Side,
    progress: impl FnMut(&Event),
) -> Result<()> {
    let side = match side {
        Side::Client => "client",
        Side::Server => "server",
    };
    run(
        executable,
        "sync",
        &[
            source.as_os_str(),
            target.as_os_str(),
            OsStr::new(side),
            OsStr::new("--cleanup"),
        ],
        progress,
    )?;
    Ok(())
}

fn run(
    executable: &Path,
    subcommand: &str,
    args: &[&OsStr],
    progress: impl FnMut(&Event),
) -> Result<Event> {
    run_with_timeout(
        executable,
        subcommand,
        args,
        progress,
        Duration::from_secs(30 * 60),
    )
}

fn run_with_timeout(
    executable: &Path,
    subcommand: &str,
    args: &[&OsStr],
    mut progress: impl FnMut(&Event),
    timeout: Duration,
) -> Result<Event> {
    let mut command = Command::new(executable);
    command
        .arg("--jsonl")
        .arg(subcommand)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child =
        BkmPwChild::spawn(command).with_context(|| format!("starting bkmpw {subcommand}"))?;
    let stdout = child.stdout().context("bkmpw stdout unavailable")?;
    let child = Arc::new(std::sync::Mutex::new(child));
    let deadline = Instant::now() + timeout;
    let (watchdog_cancel, watchdog_signal) = mpsc::channel();
    let watchdog_child = Arc::clone(&child);
    let watchdog = std::thread::spawn(move || {
        if matches!(
            watchdog_signal.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            Err(RecvTimeoutError::Timeout)
        ) && let Ok(mut child) = watchdog_child.lock()
        {
            child.terminate();
        }
    });
    let (sender, receiver) = mpsc::sync_channel::<io::Result<Vec<u8>>>(32);
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader_thread = std::thread::spawn(move || read_events(stdout, sender, reader_stop));
    let mut callback_panic = None;
    let result = (|| -> Result<Event> {
        let mut last = None;
        let mut expected_sequence = 1;
        let mut started = false;
        let mut terminal = false;
        let mut descendants_stopped = false;
        let mut post_exit_deadline = None;
        loop {
            if Instant::now() >= deadline {
                bail!("bkmpw {subcommand} timed out");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(result) => {
                    if Instant::now() >= deadline {
                        bail!("bkmpw {subcommand} timed out");
                    }
                    result?
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !descendants_stopped && child.lock().unwrap().has_exited_without_reap()? {
                        child.lock().unwrap().terminate();
                        descendants_stopped = true;
                        post_exit_deadline = Some(Instant::now() + Duration::from_millis(250));
                    }
                    if post_exit_deadline.is_some_and(|limit| Instant::now() >= limit) {
                        break;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => bail!("bkmpw machine reader stopped"),
            };
            if line.is_empty() {
                if Instant::now() >= deadline {
                    bail!("bkmpw {subcommand} timed out");
                }
                break;
            }
            if expected_sequence > 100_000 {
                bail!("bkmpw emitted too many machine events");
            }
            if line.len() as u64 > MAX_EVENT_LINE || !line.ends_with(b"\n") {
                bail!("bkmpw emitted an oversized or unterminated machine event");
            }
            let event: Event =
                serde_json::from_slice(&line).context("parsing bkmpw machine event")?;
            if event.protocol_version != 1
                || event.command != subcommand
                || event.sequence != expected_sequence
                || terminal
            {
                bail!("bkmpw machine event sequence or protocol mismatch");
            }
            expected_sequence += 1;
            match event.event.as_str() {
                "started" if !started && expected_sequence == 2 => started = true,
                "progress" if started => {
                    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| progress(&event)))
                    {
                        callback_panic = Some(payload);
                        bail!("bkmpw progress callback panicked");
                    }
                    if Instant::now() >= deadline {
                        bail!("bkmpw {subcommand} timed out");
                    }
                }
                "completed" if started => {
                    terminal = true;
                    last = Some(event);
                }
                "failed" if started => {
                    terminal = true;
                    last = Some(event);
                    child.lock().unwrap().terminate();
                    break;
                }
                _ => bail!("unexpected bkmpw machine event: {}", event.event),
            }
        }
        if !terminal {
            bail!("bkmpw machine stream ended before completion");
        }
        if Instant::now() >= deadline {
            bail!("bkmpw {subcommand} timed out");
        }
        loop {
            if child.lock().unwrap().has_exited_without_reap()? {
                break;
            }
            if Instant::now() >= deadline {
                bail!("bkmpw {subcommand} timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if Instant::now() >= deadline {
            bail!("bkmpw {subcommand} timed out");
        }
        last.context("bkmpw emitted no terminal event")
    })();
    // Stop any descendants while the Unix group leader is still unreaped.
    child.lock().unwrap().terminate();
    let _ = watchdog_cancel.send(());
    let _ = watchdog.join();
    let status = child.lock().unwrap().wait();
    stop.store(true, Ordering::Release);
    drop(receiver);
    let _ = reader_thread.join();
    if let Some(payload) = callback_panic {
        panic::resume_unwind(payload);
    }
    let status = status?;
    let last = result?;
    if !status.success() || last.event != "completed" || last.ok != Some(true) {
        if let Some(error) = &last.error {
            bail!(
                "bkmpw {} failed ({}): {}",
                subcommand,
                error.code,
                error.message
            );
        }
        bail!("bkmpw {subcommand} failed with {status}");
    }
    Ok(last)
}

fn read_events(
    mut stdout: ChildStdout,
    sender: SyncSender<io::Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            let _ = sender.send(Err(io::Error::last_os_error()));
            return;
        }
    }
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    while !stop.load(Ordering::Acquire) {
        #[cfg(windows)]
        if !pipe_ready(&stdout) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        match stdout.read(&mut chunk) {
            Ok(0) => {
                if !line.is_empty() && sender.send(Ok(line)).is_err() {
                    return;
                }
                let _ = sender.send(Ok(Vec::new()));
                return;
            }
            Ok(count) => {
                for &byte in &chunk[..count] {
                    line.push(byte);
                    if (byte == b'\n' || line.len() as u64 > MAX_EVENT_LINE)
                        && sender.send(Ok(std::mem::take(&mut line))).is_err()
                    {
                        return;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        }
    }
}

#[cfg(windows)]
fn pipe_ready(stdout: &ChildStdout) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available = 0u32;
    let result = unsafe {
        PeekNamedPipe(
            stdout.as_raw_handle() as _,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    result == 0 || available > 0
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn completed_without_started_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-bkmpw");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"completed\",\"ok\":true}'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            verify_protocol(&script)
                .unwrap_err()
                .to_string()
                .contains("unexpected bkmpw machine event")
        );
    }

    #[cfg(unix)]
    #[test]
    fn stalled_partial_line_and_stalled_exit_time_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        for (index, body) in [
            "sleep 1 & wait",
            "printf partial; sleep 1 & wait",
            "printf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true}'; exec 1>&-; sleep 1 & wait",
        ].into_iter().enumerate() {
            let script = dir.path().join(format!("fake-bkmpw-{index}"));
            std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let error = run_with_timeout(
                &script,
                "protocol-version",
                &[],
                |_| {},
                Duration::from_millis(80),
            )
            .unwrap_err();
            assert!(error.to_string().contains("timed out"), "{error:#}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_event_with_flooded_stdout_does_not_deadlock_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("flood-bkmpw");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' invalid\nwhile :; do printf '%s\\n' flood; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            run_with_timeout(
                &script,
                "protocol-version",
                &[],
                |_| {},
                Duration::from_secs(2)
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn progress_callback_panic_stops_the_process() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("progress-bkmpw");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"progress\"}'\nsleep 1 & wait\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        let result = panic::catch_unwind(|| {
            run_with_timeout(
                &script,
                "protocol-version",
                &[],
                |_| panic!("caller"),
                Duration::from_secs(2),
            )
        });
        assert!(result.is_err(), "{result:?}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_descendant_cannot_block_error_return() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("escaped-bkmpw");
        std::fs::write(
            &script,
            "#!/bin/sh\nsetsid sleep 1 &\nprintf '%s\\n' invalid\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        assert!(
            run_with_timeout(
                &script,
                "protocol-version",
                &[],
                |_| {},
                Duration::from_millis(100)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn queued_completion_cannot_hide_slow_callback_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("queued-bkmpw");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"progress\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":3,\"event\":\"completed\",\"ok\":true}'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let error = run_with_timeout(
            &script,
            "protocol-version",
            &[],
            |_| std::thread::sleep(Duration::from_millis(100)),
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watchdog_kills_child_while_progress_callback_is_blocked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("blocked-bkmpw");
        let pid_file = dir.path().join("pid");
        std::fs::write(&script, "#!/bin/sh\necho $$ > \"$3\"\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"progress\"}'\nsleep 5 & wait\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let release = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&release);
        let pid_arg = pid_file.clone();
        let worker = std::thread::spawn(move || {
            run_with_timeout(
                &script,
                "protocol-version",
                &[pid_arg.as_os_str()],
                |_| {
                    while !signal.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                },
                Duration::from_millis(100),
            )
        });
        std::thread::sleep(Duration::from_millis(250));
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let status = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let stopped = status.is_empty() || status.split(' ').nth(2) == Some("Z");
        release.store(true, Ordering::Release);
        let _ = worker.join().unwrap();
        assert!(stopped, "bkmpw child was still running after deadline");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_event_kills_background_writer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("failed-bkmpw");
        let marker = dir.path().join("writer");
        std::fs::write(&script, "#!/bin/sh\n(while :; do echo tick >> \"$3\"; sleep 0.02; done) </dev/null >/dev/null 2>&1 &\necho $! > \"$3.pid\"\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"failed\",\"ok\":false,\"error\":{\"code\":\"TEST\",\"message\":\"failed\"}}'\nexit 2\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let error = run_with_timeout(
            &script,
            "protocol-version",
            &[marker.as_os_str()],
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("TEST"), "{error:#}");
        let pid: i32 = std::fs::read_to_string(format!("{}.pid", marker.display()))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut stopped = false;
        for _ in 0..20 {
            let proc_status =
                std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            stopped = proc_status.is_empty() || proc_status.split(' ').nth(2) == Some("Z");
            if stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        assert!(stopped, "failed sync left writer running");
    }

    #[cfg(unix)]
    #[test]
    fn failed_event_ends_stalled_process_without_losing_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("failed-stall-bkmpw");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"failed\",\"ok\":false,\"error\":{\"code\":\"INTENTIONAL\",\"message\":\"failed\"}}'\nsleep 5 & wait\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        let error = run_with_timeout(
            &script,
            "protocol-version",
            &[],
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("INTENTIONAL"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completed_event_stops_background_writer_before_return() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("completed-bkmpw");
        let marker = dir.path().join("writer");
        std::fs::write(&script, "#!/bin/sh\n(while :; do echo tick >> \"$3\"; sleep 0.02; done) </dev/null >/dev/null 2>&1 &\necho $! > \"$3.pid\"\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true}'\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        run_with_timeout(
            &script,
            "protocol-version",
            &[marker.as_os_str()],
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap();
        let pid: i32 = std::fs::read_to_string(format!("{}.pid", marker.display()))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut stopped = false;
        for _ in 0..20 {
            let proc_status =
                std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            stopped = proc_status.is_empty() || proc_status.split(' ').nth(2) == Some("Z");
            if stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        assert!(stopped, "completed sync left writer running");
    }

    #[cfg(unix)]
    #[test]
    fn completed_parent_with_inherited_stdout_does_not_wait_for_descendant() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("inherited-stdout-bkmpw");
        std::fs::write(&script, "#!/bin/sh\nsleep 5 &\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true}'\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        run_with_timeout(
            &script,
            "protocol-version",
            &[],
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_parent_with_inherited_stdout_fails_promptly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("incomplete-stdout-bkmpw");
        std::fs::write(&script, "#!/bin/sh\nsleep 5 &\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}'\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        let error = run_with_timeout(
            &script,
            "protocol-version",
            &[],
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("before completion"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completed_parent_with_escaped_stdout_returns_after_bounded_drain() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("escaped-completed-bkmpw");
        let pid_file = dir.path().join("pid");
        std::fs::write(&script, "#!/bin/sh\nsetsid sleep 5 &\necho $! > \"$3\"\nprintf '%s\\n' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}' '{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true}'\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        run_with_timeout(
            &script,
            "protocol-version",
            &[pid_file.as_os_str()],
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap();
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
