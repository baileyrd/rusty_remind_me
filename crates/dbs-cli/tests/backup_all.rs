//! Integration tests for `dbs backup --all --only-due` (issue #65).
//!
//! Uses a real `SqliteStorage` (same crate `dbs-cli` already depends
//! on) to seed genuine run history with real `Utc::now()`-based
//! timestamps between CLI invocations — the only way to exercise the
//! "some sources not due" case, since `is_due` compares against the
//! real wall clock and no connector exists yet to produce that history
//! through an actual backup run.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use dbs_core::{BatchResult, SqliteStorage, Storage};

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-backup-all-test-{label}-{}",
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

/// Seeds a just-now-completed run for `source_name`, so `is_due`
/// reports it as not-due under the default `daily` schedule (20h
/// slack) on the very next check.
fn seed_recent_run(dir: &std::path::Path, source_name: &str) {
    let mut storage = SqliteStorage::open(dir.join("dbs.sqlite3").to_str().unwrap()).unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source(source_name, "raindrop", "raindrop", "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, "raindrop", "incremental", None)
        .unwrap();
    storage
        .finish_run(
            run_id,
            "success",
            &BatchResult::default(),
            0,
            None,
            None,
            &[],
        )
        .unwrap();
}

#[test]
fn all_sources_due_when_none_have_ever_run() {
    let dir = temp_dir("all-due");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = true\n",
    );

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--only-due")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  a "), "{stdout}");
    assert!(stdout.contains("  b "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_recently_run_source_is_skipped_while_a_never_run_one_is_not() {
    let dir = temp_dir("some-due");
    let config_path = write_config(
        &dir,
        "[sources.fresh]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.stale]\ntype = \"raindrop\"\nenabled = true\n",
    );
    // "fresh" just ran (seeded directly); "stale" has never run.
    seed_recent_run(&dir, "fresh");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--only-due")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  stale "), "{stdout}");
    assert!(!stdout.contains("  fresh "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn without_only_due_every_enabled_source_runs_regardless_of_history() {
    let dir = temp_dir("ignore-due");
    let config_path = write_config(
        &dir,
        "[sources.fresh]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_recent_run(&dir, "fresh");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  fresh "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_source_list_produces_no_results_and_exits_zero() {
    let dir = temp_dir("empty");
    let config_path = write_config(&dir, "");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--only-due")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), "Backup results:");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn disabled_sources_never_appear_even_when_due() {
    let dir = temp_dir("disabled");
    let config_path = write_config(
        &dir,
        "[sources.off]\ntype = \"raindrop\"\nenabled = false\n",
    );

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--only-due")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), "Backup results:");

    std::fs::remove_dir_all(&dir).ok();
}
