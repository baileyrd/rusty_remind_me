//! Integration tests for `dbs status` and `dbs history` (issue #68).
//!
//! Uses a real `SqliteStorage` (same crate `dbs-cli` already depends on)
//! to seed genuine run rows directly — no connector-candidate discovery
//! exists yet (#85-100), so a real `dbs backup` run can never reach a
//! working connector to produce this history itself.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use dbs_core::{BatchResult, SqliteStorage, Storage};

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-status-history-test-{label}-{}",
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

fn seed_run(dir: &std::path::Path, source_name: &str, status: &str) {
    let mut storage = SqliteStorage::open(dir.join("dbs.sqlite3").to_str().unwrap()).unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source(source_name, "raindrop", "raindrop", "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, "raindrop", "incremental", None)
        .unwrap();
    storage
        .finish_run(run_id, status, &BatchResult::default(), 0, None, None, &[])
        .unwrap();
    // backup_source increments this separately from the run row itself,
    // in its best-effort cleanup step — match that here too.
    storage.increment_run_count(source.id).unwrap();
}

fn run(
    dir: &std::path::Path,
    config_path: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(dbs_bin())
        .current_dir(dir)
        .arg("--config")
        .arg(config_path)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn status_with_no_sources_configured_prints_a_message() {
    let dir = temp_dir("status-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), "No sources configured.");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn status_lists_every_configured_source_with_no_runs_yet() {
    let dir = temp_dir("status-multi");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = false\n",
    );

    let output = run(&dir, &config_path, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("a") && stdout.contains("on"), "{stdout}");
    assert!(stdout.contains("b") && stdout.contains("off"), "{stdout}");
    assert!(stdout.contains("runs=0"), "{stdout}");
    assert!(stdout.contains("last=-"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn status_reflects_a_seeded_run() {
    let dir = temp_dir("status-seeded");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_run(&dir, "a", "success");

    let output = run(&dir, &config_path, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("runs=1"), "{stdout}");
    assert!(stdout.contains("last=success"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn status_for_an_unknown_source_name_is_a_placeholder_row_not_an_error() {
    let dir = temp_dir("status-unknown");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["status", "nonexistent"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("nonexistent"), "{stdout}");
    assert!(stdout.contains("off"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn status_json_emits_a_valid_json_array() {
    let dir = temp_dir("status-json");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_run(&dir, "a", "success");

    let output = run(&dir, &config_path, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "a");
    assert_eq!(arr[0]["run_count"], 1);
    assert_eq!(arr[0]["last_run_status"], "success");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn history_with_no_runs_prints_nothing() {
    let dir = temp_dir("history-empty");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(&dir, &config_path, &["history"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.trim().is_empty(), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn history_lists_seeded_runs_newest_first() {
    let dir = temp_dir("history-multi");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_run(&dir, "a", "success");
    seed_run(&dir, "b", "failed");

    let output = run(&dir, &config_path, &["history"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.starts_with(' ')).collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    // "b" ran after "a", so it sorts first (started_at DESC).
    assert!(lines[0].contains(" b "), "{stdout}");
    assert!(lines[0].contains("failed"), "{stdout}");
    assert!(lines[1].contains(" a "), "{stdout}");
    assert!(lines[1].contains("success"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn history_limit_flag_caps_the_count() {
    let dir = temp_dir("history-limit");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    for _ in 0..3 {
        seed_run(&dir, "a", "success");
    }

    let output = run(&dir, &config_path, &["history", "-n", "2"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.starts_with(' ')).collect();
    assert_eq!(lines.len(), 2, "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn history_filters_by_source_name() {
    let dir = temp_dir("history-filter");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_run(&dir, "a", "success");
    seed_run(&dir, "b", "failed");

    let output = run(&dir, &config_path, &["history", "a"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(" a "), "{stdout}");
    assert!(!stdout.contains(" b "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn history_json_emits_a_valid_json_array() {
    let dir = temp_dir("history-json");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_run(&dir, "a", "success");

    let output = run(&dir, &config_path, &["history", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["source_name"], "a");
    assert_eq!(arr[0]["status"], "success");

    std::fs::remove_dir_all(&dir).ok();
}
