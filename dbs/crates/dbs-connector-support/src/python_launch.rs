//! A shared launcher for the Python/Playwright subprocess the
//! browser-automation connectors (`reddit`, `skool`, `youtube`) and
//! `dbs capture` will delegate to.
//!
//! Mirrors `dbs.connectors._playwright`'s *role* in the reference,
//! not its code: that module's `launch_scrubbed_context` drives
//! Playwright **in-process** (it takes an already-imported
//! `playwright.sync_api` handle and calls
//! `pw.chromium.launch_persistent_context(...)` directly), which has
//! no meaning in Rust — there's no Rust Playwright binding, and
//! per this port's round-1 decision (gap-analysis.md's Connectors
//! cluster, decision 3) there isn't going to be one. Instead, a
//! browser-automation connector here shells out to a **separate**
//! Python script that imports Playwright itself and does the actual
//! browser driving; this module is the generic, Playwright-agnostic
//! half of that split — interpreter resolution, a stall/wall-clock
//! timeout (a hung browser launch must not block a scheduled run
//! forever, the same concern [`crate::run_with_watchdog`] exists
//! for), and output capture. It knows nothing about what script it's
//! running or what arguments that script expects; each connector
//! that uses it owns its own script and argument contract.
//!
//! Not part of `dbs-core`'s public contract, same as this crate's
//! other helpers — free to change without a `CORE_API_VERSION` bump.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::watchdog::{run_with_watchdog, WatchdogError};

/// Why launching a Python subprocess failed.
#[derive(Debug)]
pub enum PythonLaunchError {
    /// No `python3`/`python` interpreter found on `PATH`.
    NoInterpreter,
    /// The interpreter (or the script under it) never started.
    Spawn(std::io::Error),
    /// The process ran past its timeout without completing and was
    /// abandoned — never force-killed (Rust threads can't be, the
    /// same constraint [`crate::run_with_watchdog`] documents), so it
    /// may still be running detached.
    Timeout(String),
    /// The watchdog's worker thread panicked before it could report
    /// a result.
    WorkerPanicked,
}

impl std::fmt::Display for PythonLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoInterpreter => write!(f, "no python3/python found on PATH"),
            Self::Spawn(e) => write!(f, "{e}"),
            Self::Timeout(msg) => write!(f, "{msg}"),
            Self::WorkerPanicked => write!(f, "watchdog worker thread panicked"),
        }
    }
}

impl std::error::Error for PythonLaunchError {}

/// Finds a Python interpreter on `PATH` — tries `python3` before
/// `python`, matching most systems' convention of `python3` being
/// the unambiguous name (the same resolution `dbs-cli`'s
/// `update-ytdlp` command uses).
pub fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|bin| {
        Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Runs `<python> <script> <args...>` to completion, using whichever
/// interpreter [`find_python`] resolves. See
/// [`run_python_script_using`] for the testable, interpreter-
/// injectable form this delegates to.
pub fn run_python_script(
    script: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<Output, PythonLaunchError> {
    let python = find_python().ok_or(PythonLaunchError::NoInterpreter)?;
    run_python_script_using(python, script, args, timeout)
}

/// Runs `<interpreter> <script> <args...>` to completion, abandoning
/// it past `timeout` (zero disables the watchdog — the call then
/// blocks until the process exits, however long that takes).
/// Captures stdout/stderr verbatim; the caller interprets them (a
/// browser-automation script might emit a JSON result line, for
/// instance) — this function has no opinion on that format.
///
/// Split out from [`run_python_script`] so tests (and, in principle,
/// a caller with unusual interpreter-resolution needs) can supply the
/// interpreter directly instead of depending on what's actually on
/// `PATH`.
pub fn run_python_script_using(
    interpreter: &str,
    script: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<Output, PythonLaunchError> {
    let mut cmd = Command::new(interpreter);
    cmd.arg(script).args(args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().map_err(PythonLaunchError::Spawn)?;

    let description = format!("python script {}", script.display());
    let result = run_with_watchdog(
        move || child.wait_with_output(),
        timeout,
        &description,
        None,
    );
    match result {
        Ok(output) => Ok(output),
        Err(WatchdogError::Timeout(t)) => Err(PythonLaunchError::Timeout(t.to_string())),
        Err(WatchdogError::Inner(e)) => Err(PythonLaunchError::Spawn(e)),
        Err(WatchdogError::WorkerPanicked) => Err(PythonLaunchError::WorkerPanicked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-connector-support-python-launch-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stub "script" run through `/bin/sh` standing in for a real
    /// Python interpreter — `run_python_script_using` doesn't care
    /// what the interpreter is, so a shell script exercises the
    /// launcher without depending on Python actually being present.
    fn write_stub_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "{body}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn find_python_resolves_without_panicking() {
        // Environment-dependent (CI may or may not have python3/python
        // on PATH); just prove the resolution itself doesn't panic and
        // returns one of the two known names when it does find one.
        if let Some(bin) = find_python() {
            assert!(bin == "python3" || bin == "python");
        }
    }

    #[test]
    fn run_python_script_using_captures_stdout_and_a_zero_exit_code() {
        let dir = temp_dir("stdout");
        let script = write_stub_script(&dir, "stub.sh", "echo hello-from-stub");
        let output =
            run_python_script_using("/bin/sh", &script, &[], Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello-from-stub"
        );
    }

    #[test]
    fn run_python_script_using_captures_a_nonzero_exit_code() {
        let dir = temp_dir("exit-code");
        let script = write_stub_script(&dir, "stub.sh", "echo boom >&2\nexit 7");
        let output =
            run_python_script_using("/bin/sh", &script, &[], Duration::from_secs(5)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "boom");
    }

    #[test]
    fn run_python_script_using_passes_arguments_through() {
        let dir = temp_dir("args");
        let script = write_stub_script(&dir, "stub.sh", "echo \"$1-$2\"");
        let output = run_python_script_using(
            "/bin/sh",
            &script,
            &["a".to_string(), "b".to_string()],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "a-b");
    }

    #[test]
    fn run_python_script_using_with_a_missing_interpreter_is_a_spawn_error() {
        let dir = temp_dir("missing-interpreter");
        let script = write_stub_script(&dir, "stub.sh", "echo unreachable");
        let result = run_python_script_using(
            "/nonexistent/interpreter/binary",
            &script,
            &[],
            Duration::from_secs(5),
        );
        assert!(
            matches!(result, Err(PythonLaunchError::Spawn(_))),
            "{result:?}"
        );
    }

    #[test]
    fn run_python_script_using_abandons_a_stalled_process_past_its_timeout() {
        let dir = temp_dir("timeout");
        let script = write_stub_script(&dir, "stub.sh", "sleep 5\necho too-late");
        let result = run_python_script_using("/bin/sh", &script, &[], Duration::from_millis(100));
        assert!(
            matches!(result, Err(PythonLaunchError::Timeout(_))),
            "{result:?}"
        );
    }

    #[test]
    fn run_python_script_using_with_a_zero_timeout_runs_inline_to_completion() {
        let dir = temp_dir("zero-timeout");
        let script = write_stub_script(&dir, "stub.sh", "echo done");
        let output = run_python_script_using("/bin/sh", &script, &[], Duration::ZERO).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");
    }
}
