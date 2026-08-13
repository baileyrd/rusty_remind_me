//! Integration tests for `dbs backup`'s single-source path (issue #64).
//!
//! `--all` is out of scope for this issue (a later follow-up, #65-#67)
//! and is only checked here to confirm it still reports as a stub
//! rather than silently doing the wrong thing.
//!
//! **On "a successful run":** no connector-candidate discovery
//! mechanism exists yet (an implicit connectors-cluster prerequisite,
//! #85-100), so `dbs backup <configured-source>` can never actually
//! reach a working connector today — every registered source's type
//! is reported "not found". A disabled source, though, returns
//! `Ok(RunResult)` from `BackupService::backup_source` *before* any
//! registry lookup happens, so it exercises the exact same
//! `Ok(...)` → print → exit-0 path a real success would use, just
//! with a `skipped` terminal status instead of `success` — the
//! honest way to cover "the command completes successfully
//! end-to-end" without a real connector to succeed against.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-backup-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(dir: &std::path::Path, sources_block: &str) -> PathBuf {
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "export_dir = \"exports\"").unwrap();
    writeln!(file, "download_root = \"downloads\"").unwrap();
    writeln!(file, "{sources_block}").unwrap();
    config_path
}

#[test]
fn unknown_source_name_exits_5() {
    let dir = temp_dir("unknown-source");
    let config_path = write_config(&dir, "");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("nonexistent-source")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown source"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unregistered_connector_type_surfaces_as_a_config_error() {
    let dir = temp_dir("unregistered-connector");
    let config_path = write_config(
        &dir,
        "[sources.raindrop]\ntype = \"raindrop\"\nenabled = true\n",
    );

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("raindrop")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("connector"));
    assert!(stderr.contains("not found"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_disabled_source_completes_successfully_and_prints_a_result() {
    let dir = temp_dir("disabled-source");
    let config_path = write_config(
        &dir,
        "[sources.raindrop]\ntype = \"raindrop\"\nenabled = false\n",
    );

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("raindrop")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Backup results:"));
    assert!(stdout.contains("raindrop"));
    assert!(stdout.contains("skipped"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_source_and_no_all_flag_is_a_usage_error() {
    let dir = temp_dir("no-source");
    let config_path = write_config(&dir, "");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Specify a SOURCE name or --all"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn backup_all_is_still_a_stub() {
    let dir = temp_dir("all-stub");
    let config_path = write_config(&dir, "");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not yet implemented"));

    std::fs::remove_dir_all(&dir).ok();
}
