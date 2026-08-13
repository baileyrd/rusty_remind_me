//! Integration tests for `dbs update-ytdlp` (issue #73).
//!
//! Never invokes a real `pip install` (slow, network-dependent, and
//! would actually mutate the test runner's Python environment).
//! Instead: `--dry-run` never executes anything, and the "successful
//! update"/"install failed" paths use a fake `python3` script placed
//! first on `PATH` — real enough to exercise `find_python`'s
//! `--version` probe and this command's exit-code/message branching,
//! without touching the network or a real interpreter.

use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-update-ytdlp-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes an executable shell script named `python3` into `dir` that
/// exits 0 for `--version` (so `find_python` finds it) and `exit_code`
/// for anything else (the simulated `pip install` call).
#[cfg(unix)]
fn write_fake_python(dir: &std::path::Path, exit_code: i32) {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("python3");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nexit {exit_code}\n"),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
}

#[test]
fn dry_run_prints_the_command_and_never_executes_anything() {
    let dir = temp_dir("dry-run");
    // A real interpreter (or none) doesn't matter — dry-run never gets
    // as far as running it.
    let output = Command::new(dbs_bin())
        .arg("update-ytdlp")
        .arg("--dry-run")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("pip install --upgrade yt-dlp[default]"),
        "{stdout}"
    );
    assert!(!stdout.contains("upgraded"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_python_on_path_is_a_config_error() {
    let dir = temp_dir("no-python");
    // An empty directory on PATH — no python3/python to find.
    let output = Command::new(dbs_bin())
        .arg("update-ytdlp")
        .arg("--dry-run")
        .env_clear()
        .env("PATH", &dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no python3/python found"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn a_successful_pip_install_prints_the_upgraded_message() {
    let dir = temp_dir("success");
    write_fake_python(&dir, 0);

    let output = Command::new(dbs_bin())
        .arg("update-ytdlp")
        .env_clear()
        .env("PATH", &dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("yt-dlp upgraded"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn a_failing_pip_install_propagates_the_exit_code_without_the_success_message() {
    let dir = temp_dir("failure");
    write_fake_python(&dir, 7);

    let output = Command::new(dbs_bin())
        .arg("update-ytdlp")
        .env_clear()
        .env("PATH", &dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("upgraded"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
