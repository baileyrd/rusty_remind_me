//! Integration tests for `dbs backup --progress`/`--no-progress` (issue
//! #67). The rendered line's *text* can't be exercised through the
//! compiled binary — every source here reports "connector not found"
//! before `backup_source` ever reaches its `SourceStart` emission point
//! (no connector-candidate discovery exists yet, #85-100) — see
//! `ProgressRenderer`'s own unit tests in `src/main.rs` for that. These
//! just confirm the flags are accepted and don't change the exit code
//! or results table.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-backup-progress-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(dir: &std::path::Path) -> PathBuf {
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "export_dir = \"exports\"").unwrap();
    writeln!(file, "download_root = \"downloads\"").unwrap();
    writeln!(file, "[sources.a]\ntype = \"raindrop\"\nenabled = true").unwrap();
    config_path
}

#[test]
fn progress_flag_does_not_change_the_outcome() {
    let dir = temp_dir("progress");
    let config_path = write_config(&dir);

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--progress")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  a "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_progress_flag_does_not_change_the_outcome() {
    let dir = temp_dir("no-progress");
    let config_path = write_config(&dir);

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--no-progress")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  a "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn progress_and_no_progress_together_is_a_usage_error() {
    let dir = temp_dir("conflict");
    let config_path = write_config(&dir);

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--progress")
        .arg("--no-progress")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}
