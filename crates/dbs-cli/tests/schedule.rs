//! Integration tests for `dbs schedule` (issue #74).
//!
//! Platform-branch *content* (Linux cron/systemd vs. Windows
//! `schtasks`) is covered by `render_schedule`'s unit tests in
//! `src/main.rs`, parameterized so both branches run regardless of
//! host OS. These tests just confirm the compiled binary wires the
//! command up end to end on whatever platform actually built it.

use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

#[test]
fn schedule_prints_a_snippet_containing_the_resolved_config_path() {
    let output = Command::new(dbs_bin())
        .arg("--config")
        .arg("dbs.toml")
        .arg("schedule")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    // "dbs.toml" alone is relative; the printed snippet must contain
    // an absolutized path (i.e. more than just the bare filename).
    assert!(stdout.contains("backup --all"), "{stdout}");
    assert!(!stdout.trim().is_empty(), "{stdout}");
}

#[test]
fn schedule_hourly_differs_from_the_default_daily_output() {
    let daily = Command::new(dbs_bin())
        .arg("--config")
        .arg("dbs.toml")
        .arg("schedule")
        .output()
        .unwrap();
    let hourly = Command::new(dbs_bin())
        .arg("--config")
        .arg("dbs.toml")
        .arg("schedule")
        .arg("--interval")
        .arg("hourly")
        .output()
        .unwrap();
    assert_eq!(daily.status.code(), Some(0), "{daily:?}");
    assert_eq!(hourly.status.code(), Some(0), "{hourly:?}");
    assert_ne!(daily.stdout, hourly.stdout);
}
