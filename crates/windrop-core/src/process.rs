//! Process execution.
//!
//! Two things matter here:
//!
//! * [`CommandSpec`] is *pure data*. Environment builders and sandbox builders
//!   produce specs, and the test-suite asserts on them without ever launching a
//!   process. That is what makes the hard parts of the compatibility layer
//!   testable on a machine with no Wine installed.
//! * [`CommandSpec::run_capture`] and [`CommandSpec::run_logged`] execute them.
//!   Both honour a timeout and kill the *entire* process tree, because Wine
//!   leaves `wineserver` and `services.exe` running when the parent dies.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// A fully-described external command. Pure data, so it can be compared,
/// printed and asserted on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Explicitly set environment variables.
    pub env: BTreeMap<OsString, OsString>,
    /// Variables removed from the inherited environment.
    pub env_remove: Vec<OsString>,
    pub cwd: Option<PathBuf>,
}

/// Result of running a command to completion.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Path to the log file, when the command was run with [`run_logged`].
    pub log_file: Option<PathBuf>,
}

// `run_logged` is documented by name above; keep the link resolvable.
#[allow(unused_imports)]
use CommandSpec as _CommandSpecDoc;

impl Output {
    pub fn success(&self) -> bool {
        self.status.success() && !self.timed_out
    }

    pub fn code(&self) -> i32 {
        self.status.code().unwrap_or(-1)
    }

    /// stdout and stderr joined, useful for diagnostics.
    pub fn combined(&self) -> String {
        let mut s = String::new();
        if !self.stdout.is_empty() {
            s.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s
    }
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        CommandSpec {
            program: program.into(),
            ..Default::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn envs<K, V, I>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        for (k, v) in vars {
            self.env.insert(k.into(), v.into());
        }
        self
    }

    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// True when an environment variable was set to a non-empty value.
    pub fn env_has(&self, key: &str) -> bool {
        self.env
            .get(OsStr::new(key))
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    pub fn env_get(&self, key: &str) -> Option<&OsStr> {
        self.env.get(OsStr::new(key)).map(|v| v.as_os_str())
    }

    /// Build a [`std::process::Command`] from the spec.
    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// Shell-style rendering for logs and error messages.
    pub fn display(&self) -> String {
        let mut out = shell_quote(&self.program.to_string_lossy());
        for (k, v) in &self.env {
            out.push(' ');
            out.push_str(&format!(
                "{}={}",
                k.to_string_lossy(),
                shell_quote(&v.to_string_lossy())
            ));
        }
        for a in &self.args {
            out.push(' ');
            out.push_str(&shell_quote(&a.to_string_lossy()));
        }
        out
    }

    fn spawn_child(&self, stdout: Stdio, stderr: Stdio) -> Result<Child> {
        let mut cmd = self.to_command();
        cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        isolate_process_group(&mut cmd);
        tracing::debug!(command = %self.display(), "spawning");
        cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ToolMissing(self.program.to_string_lossy().to_string())
            } else {
                Error::Io(e)
            }
        })
    }

    /// Run a short-lived command, capturing stdout and stderr, with a timeout.
    ///
    /// The pipes are drained on dedicated threads so a chatty child cannot
    /// deadlock by filling the pipe buffer.
    pub fn run_capture(&self, timeout: Duration) -> Result<Output> {
        let started = Instant::now();
        let mut child = self.spawn_child(Stdio::piped(), Stdio::piped())?;

        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let out_thread = out_pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
        });
        let err_thread = err_pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
        });

        let (status, timed_out) = wait_with_timeout(&mut child, timeout, started)?;
        let stdout = join_bytes(out_thread);
        let stderr = join_bytes(err_thread);

        let output = Output {
            status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out,
            log_file: None,
        };
        tracing::debug!(command = %self.display(), code = output.code(), "finished");
        Ok(output)
    }

    /// Run a command exactly once, capturing output, and fail if it does not
    /// exit zero. Convenience for `wine --version` style probes.
    pub fn run_checked(&self, timeout: Duration) -> Result<Output> {
        let out = self.run_capture(timeout)?;
        if !out.success() {
            return Err(self.failure(&out));
        }
        Ok(out)
    }

    /// Run a potentially long or noisy command (an installer, `winetricks`),
    /// sending all output to `log_file`. This avoids pipe deadlocks and gives
    /// the GUI a log to display verbatim.
    pub fn run_logged(&self, log_file: &Path, timeout: Duration) -> Result<Output> {
        if let Some(parent) = log_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)?;
        let err_file = file.try_clone()?;

        let started = Instant::now();
        tracing::info!(command = %self.display(), log = %log_file.display(), "starting (output follows)");
        let mut child = self.spawn_child(Stdio::from(file), Stdio::from(err_file))?;
        let (status, timed_out) = wait_with_timeout(&mut child, timeout, started)?;

        let text = std::fs::read_to_string(log_file).unwrap_or_default();
        let output = Output {
            status,
            stdout: text.clone(),
            stderr: String::new(),
            timed_out,
            log_file: Some(log_file.to_path_buf()),
        };
        if timed_out {
            tracing::warn!(command = %self.display(), "timed out and was killed");
        } else {
            tracing::info!(command = %self.display(), code = output.code(), "exit");
        }
        Ok(output)
    }

    /// Run with inherited stdio, for interactive installers the user must click
    /// through.
    pub fn run_interactive(&self) -> Result<Output> {
        let mut child = self.spawn_child(Stdio::inherit(), Stdio::inherit())?;
        let status = child.wait()?;
        Ok(Output {
            status,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            log_file: None,
        })
    }

    /// Construct the [`Error::CommandFailed`] / [`Error::Timeout`] that matches
    /// an unsuccessful run.
    pub fn failure(&self, out: &Output) -> Error {
        if out.timed_out {
            return Error::Timeout {
                command: self.display(),
                seconds: 0,
            };
        }
        let stderr = if out.stderr.is_empty() {
            out.stdout.clone()
        } else {
            out.stderr.clone()
        };
        Error::CommandFailed {
            command: self.display(),
            code: out.code(),
            stderr: tail(&stderr, 4000),
        }
    }
}

/// Wait for a child, killing its whole process group if it overruns.
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
    started: Instant,
) -> Result<(ExitStatus, bool)> {
    let deadline = started + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok((status, false)),
            None => {
                if Instant::now() >= deadline {
                    kill_tree(child);
                    let status = child.wait()?;
                    return Ok((status, true));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Kill the child and every process in its group.
fn kill_tree(child: &mut Child) {
    let pid = child.id() as i32;
    // SAFETY: kill(2) with a negative pid targets the process group we created
    // in `isolate_process_group`. Both arguments are plain integers.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = child.kill();
}

/// Put the child in its own session/process group so we can kill the tree.
///
/// Failure is deliberately non-fatal: if `setsid` is unavailable the command
/// still runs, we just lose tree-wide killing for that child.
#[cfg(unix)]
fn isolate_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and the only thing we do between
    // fork and exec. Errors are ignored on purpose.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                // Already a group leader: continue without a new session.
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn isolate_process_group(_cmd: &mut Command) {}

fn join_bytes(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    match handle {
        Some(h) => h.join().unwrap_or_default(),
        None => Vec::new(),
    }
}

/// The last `max` characters of `s`, for error messages.
fn tail(s: &str, max: usize) -> String {
    let trimmed = s.trim_end();
    let count = trimmed.chars().count();
    if count <= max {
        return trimmed.to_string();
    }
    trimmed.chars().skip(count - max).collect()
}

/// Quote a word so it can be pasted into a shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Locate an executable on `$PATH`. Used by the dependency doctor.
pub fn which(tool: &str) -> Option<PathBuf> {
    // An explicit path is used as-is when it exists and is a file.
    if tool.contains('/') {
        let p = PathBuf::from(tool);
        return if is_executable(&p) { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| is_executable(candidate))
}

/// True when `p` is a regular file with at least one execute bit set.
pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_echo() -> CommandSpec {
        CommandSpec::new("/bin/echo").arg("hello")
    }

    #[test]
    fn capture_reads_stdout() {
        let out = spec_echo().run_capture(Duration::from_secs(10)).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        let s = CommandSpec::new("/usr/bin/wine")
            .env("WINEPREFIX", "/home/u/my prefix")
            .arg("setup.exe");
        assert_eq!(
            s.display(),
            "/usr/bin/wine WINEPREFIX='/home/u/my prefix' setup.exe"
        );
    }

    #[test]
    fn env_is_applied_and_removable() {
        let s = CommandSpec::new("/bin/sh")
            .args(["-c", "printf %s \"$WINEDROP_TEST_VAR\""])
            .env("WINEDROP_TEST_VAR", "brand-new-application");
        let out = s.run_capture(Duration::from_secs(10)).unwrap();
        assert_eq!(out.stdout, "brand-new-application");

        let s = CommandSpec::new("/bin/sh")
            .args(["-c", "printf %s \"${WINEDROP_TEST_VAR:-absent}\""])
            .env_remove("WINEDROP_TEST_VAR");
        let out = s.run_capture(Duration::from_secs(10)).unwrap();
        assert_eq!(out.stdout, "absent");
    }

    #[test]
    fn failing_command_is_reported_with_its_output() {
        let s = CommandSpec::new("/bin/sh").args(["-c", "echo 'something exploded' >&2; exit 3"]);
        let out = s.run_capture(Duration::from_secs(10)).unwrap();
        assert!(!out.success());
        assert_eq!(out.code(), 3);
        let err = s.failure(&out);
        match err {
            Error::CommandFailed { code, stderr, .. } => {
                assert_eq!(code, 3);
                assert!(stderr.contains("something exploded"));
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn timeout_kills_a_sleeping_child_and_its_children() {
        // The shell spawns a grandchild `sleep`; killing only the shell would
        // leave the grandchild alive. Verify our group kill reaps both.
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("grandchild-slept");
        let log = tmp.path().join("out.log");
        let s = CommandSpec::new("/bin/sh").args([
            "-c",
            &format!("(sleep 3; touch {}) & sleep 30", marker.display()),
        ]);
        let started = Instant::now();
        let out = s.run_logged(&log, Duration::from_millis(600)).unwrap();
        assert!(out.timed_out, "should have timed out");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must not wait for the child"
        );

        // Give the grandchild a chance to prove it was killed.
        std::thread::sleep(Duration::from_millis(2900));
        assert!(!marker.exists(), "grandchild survived the group kill");
    }

    #[test]
    fn run_logged_tees_output_to_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("nested/dir/install.log");
        let s = CommandSpec::new("/bin/sh").args(["-c", "echo 'fixme: doing a thing'; exit 0"]);
        let out = s.run_logged(&log, Duration::from_secs(10)).unwrap();
        assert!(out.success());
        assert_eq!(out.log_file.as_deref(), Some(log.as_path()));
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("fixme: doing a thing"));
        // The captured stdout mirrors the log so callers can show it directly.
        assert!(out.stdout.contains("doing a thing"));
    }

    #[test]
    fn missing_program_reports_which_tool() {
        let s = CommandSpec::new("/definitely/not/here/binary");
        match s.run_capture(Duration::from_secs(5)) {
            Err(Error::ToolMissing(t)) => assert!(t.contains("binary")),
            other => panic!("expected ToolMissing, got {other:?}"),
        }
    }

    #[test]
    fn which_finds_shell_but_not_a_ghost() {
        assert!(which("sh").is_some());
        assert!(which("windrop-definitely-not-installed").is_none());
    }

    #[test]
    fn which_rejects_non_executable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain.txt");
        std::fs::write(&plain, "not executable").unwrap();
        assert!(which(plain.to_str().unwrap()).is_none());
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn tail_keeps_the_end_of_long_output() {
        let s = "a".repeat(100) + "END";
        assert_eq!(tail(&s, 5), "aaEND");
    }
}
