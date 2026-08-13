//! Integration tests for the `dbs` binary's skeleton (issue #63):
//! `--help` output, an unrecognized subcommand's error, and `dbs init`
//! on a fresh directory (including the no-clobber re-run).
//!
//! Runs the real compiled binary via `CARGO_BIN_EXE_dbs`, same pattern
//! as `dbs-core`'s `test_connector_fixture` integration tests.

use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-integration-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn help_lists_every_subcommand() {
    let output = Command::new(dbs_bin()).arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Daily Backup System"));
    for name in [
        "init",
        "backup",
        "status",
        "history",
        "items",
        "stats",
        "export",
        "export-notes",
        "export-profiles",
        "export-wiki",
        "verify",
        "restore",
        "decrypt",
        "doctor",
        "update-ytdlp",
        "maintain",
        "schedule",
        "serve",
        "capture",
        "version",
        "sources",
        "connectors",
        "research",
    ] {
        assert!(stdout.contains(name), "missing subcommand: {name}");
    }
}

#[test]
fn unknown_subcommand_errors_with_a_nonzero_exit() {
    let output = Command::new(dbs_bin())
        .arg("this-is-not-a-real-subcommand")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unrecognized subcommand"));
}

#[test]
fn version_flag_prints_a_version() {
    let output = Command::new(dbs_bin()).arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.trim_start().starts_with("dbs "));
}

#[test]
fn init_on_a_fresh_directory_writes_config_env_example_and_database() {
    let dir = temp_dir("fresh-init");
    let config_path = dir.join("dbs.toml");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("init")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Wrote"));
    assert!(stdout.contains("Initialized database"));

    assert!(config_path.is_file());
    assert!(dir.join(".env.example").is_file());
    assert!(dir.join("dbs.sqlite3").is_file());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn init_rerun_does_not_clobber_the_existing_config() {
    let dir = temp_dir("no-clobber-init");
    let config_path = dir.join("dbs.toml");

    let first = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("init")
        .output()
        .unwrap();
    assert!(first.status.success());

    let second = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("init")
        .output()
        .unwrap();
    assert!(second.status.success());
    let stdout = String::from_utf8(second.stdout).unwrap();
    assert!(stdout.contains("already exists"));
    assert!(stdout.contains("--force"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn init_force_overwrites_the_config() {
    let dir = temp_dir("force-init");
    let config_path = dir.join("dbs.toml");
    std::fs::write(&config_path, "# stale\n").unwrap();

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("init")
        .arg("--force")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let contents = std::fs::read_to_string(&config_path).unwrap();
    assert_ne!(contents, "# stale\n");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_stub_subcommand_reports_not_yet_implemented() {
    // `backup` (#64), `status`/`history` (#68), `items`/`stats` (#69),
    // `export*`/`decrypt` (#70), `sources`/`connectors` (#71),
    // `capture` (#76), and `research` (#77) are no longer pure stubs —
    // `verify` still is. There's no remaining nested-subcommand enum
    // (`sources`/`connectors`/`research`) that's still a pure stub, so
    // there's nothing left to cover with a "nested stub" variant of
    // this test.
    let output = Command::new(dbs_bin()).arg("verify").output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not yet implemented"));
}
