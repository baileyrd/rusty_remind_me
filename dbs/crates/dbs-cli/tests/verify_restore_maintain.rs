//! Integration tests for `dbs verify`, `dbs restore`, and `dbs
//! maintain` (issue #195) — all three were previously CLI stubs
//! despite `BackupService::verify`/`restore` (and, since #195, the
//! new `BackupService::maintain`) being complete and unit-tested.
//!
//! Uses a real `SqliteStorage` to seed genuine item rows directly —
//! same convention as `export_decrypt.rs` — since no connector-
//! candidate discovery exists to produce data through a real `dbs
//! backup` run.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use dbs_core::{PreparedItem, SqliteStorage, Storage};

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-verify-restore-maintain-test-{label}-{}",
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

fn prepared(external_id: &str, title: &str) -> PreparedItem {
    PreparedItem {
        external_id: external_id.to_string(),
        item_kind: "bookmark".to_string(),
        title: Some(title.to_string()),
        url: Some(format!("https://example.com/{external_id}")),
        body: Some("body text".to_string()),
        tags: vec!["rust".to_string()],
        item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
        item_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        content_hash: format!("hash-{external_id}"),
        raw_json: "{}".to_string(),
        deleted: false,
        media: Vec::new(),
    }
}

fn seed_items(dir: &std::path::Path, source_name: &str, items: &[PreparedItem]) {
    let mut storage = SqliteStorage::open(dir.join("dbs.sqlite3").to_str().unwrap()).unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source(source_name, "raindrop", "raindrop", "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, "raindrop", "incremental", None)
        .unwrap();
    storage
        .upsert_items(source.id, run_id, items, false, 0)
        .unwrap();
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

// -- verify ---------------------------------------------------------------

#[test]
fn verify_on_an_empty_database_reports_ok() {
    let dir = temp_dir("verify-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["verify"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("OK"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn verify_archive_on_a_real_exported_bundle_reports_verified_count() {
    let dir = temp_dir("verify-archive-ok");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One")]);
    let export = run(
        &dir,
        &config_path,
        &["export", "--out", "out.zip", "--format", "archive"],
    );
    assert_eq!(export.status.code(), Some(0), "{export:?}");

    let output = run(&dir, &config_path, &["verify", "--archive", "out.zip"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("OK"), "{stdout}");
    assert!(stdout.contains("1 entr"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn verify_archive_on_a_nonexistent_file_is_a_config_error() {
    let dir = temp_dir("verify-archive-missing");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["verify", "--archive", "nope.zip"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}

// -- restore ----------------------------------------------------------------

#[test]
fn restore_replays_an_exported_archive_into_a_fresh_database() {
    let src_dir = temp_dir("restore-src");
    let src_config = write_config(
        &src_dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_items(&src_dir, "a", &[prepared("1", "Item One")]);
    let export = run(
        &src_dir,
        &src_config,
        &["export", "--out", "out.zip", "--format", "archive"],
    );
    assert_eq!(export.status.code(), Some(0), "{export:?}");

    let dst_dir = temp_dir("restore-dst");
    let dst_config = write_config(&dst_dir, "");
    let bundle = src_dir.join("out.zip");

    let output = run(
        &dst_dir,
        &dst_config,
        &["restore", bundle.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Restored 1 item(s)"), "{stdout}");
    assert!(stdout.contains("+1 created"), "{stdout}");

    std::fs::remove_dir_all(&src_dir).ok();
    std::fs::remove_dir_all(&dst_dir).ok();
}

#[test]
fn restore_dry_run_reports_what_it_would_do_and_writes_nothing() {
    let src_dir = temp_dir("restore-dry-src");
    let src_config = write_config(
        &src_dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_items(&src_dir, "a", &[prepared("1", "Item One")]);
    let export = run(
        &src_dir,
        &src_config,
        &["export", "--out", "out.zip", "--format", "archive"],
    );
    assert_eq!(export.status.code(), Some(0), "{export:?}");

    let dst_dir = temp_dir("restore-dry-dst");
    let dst_config = write_config(&dst_dir, "");
    let bundle = src_dir.join("out.zip");

    let output = run(
        &dst_dir,
        &dst_config,
        &["restore", bundle.to_str().unwrap(), "--dry-run"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Would restore 1 item(s)"), "{stdout}");
    assert!(!stdout.contains("created"), "{stdout}");

    let items = run(&dst_dir, &dst_config, &["items"]);
    let items_stdout = String::from_utf8(items.stdout).unwrap();
    assert!(!items_stdout.contains("Item One"), "{items_stdout}");

    std::fs::remove_dir_all(&src_dir).ok();
    std::fs::remove_dir_all(&dst_dir).ok();
}

#[test]
fn restore_json_emits_a_valid_json_object() {
    let src_dir = temp_dir("restore-json-src");
    let src_config = write_config(
        &src_dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_items(&src_dir, "a", &[prepared("1", "Item One")]);
    let export = run(
        &src_dir,
        &src_config,
        &["export", "--out", "out.zip", "--format", "archive"],
    );
    assert_eq!(export.status.code(), Some(0), "{export:?}");

    let dst_dir = temp_dir("restore-json-dst");
    let dst_config = write_config(&dst_dir, "");
    let bundle = src_dir.join("out.zip");

    let output = run(
        &dst_dir,
        &dst_config,
        &["restore", bundle.to_str().unwrap(), "--json"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["fetched"], 1);

    std::fs::remove_dir_all(&src_dir).ok();
    std::fs::remove_dir_all(&dst_dir).ok();
}

#[test]
fn restore_of_a_nonexistent_path_is_a_config_error() {
    let dir = temp_dir("restore-missing");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["restore", "nope.zip"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}

// -- maintain ---------------------------------------------------------------

#[test]
fn maintain_on_an_empty_database_checkpoints_and_skips_vacuum_by_default() {
    let dir = temp_dir("maintain-default");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["maintain"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Database:"), "{stdout}");
    assert!(stdout.contains("WAL checkpoint: ok"), "{stdout}");
    assert!(stdout.contains("vacuum:         skipped"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn maintain_vacuum_reports_the_vacuum_as_done() {
    let dir = temp_dir("maintain-vacuum");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["maintain", "--vacuum"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("vacuum:         done"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn maintain_snapshot_writes_a_real_sqlite_file_and_refuses_to_overwrite_it() {
    let dir = temp_dir("maintain-snapshot");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["maintain", "--snapshot", "snap.db"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("snapshot:"), "{stdout}");
    assert!(dir.join("snap.db").is_file());

    // A second maintain --snapshot at the same path must refuse to
    // overwrite it (mirrors Storage::vacuum_into's own existing-path
    // guard, already unit-tested at the storage layer).
    let second = run(&dir, &config_path, &["maintain", "--snapshot", "snap.db"]);
    assert_eq!(second.status.code(), Some(4), "{second:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn maintain_json_emits_a_valid_json_object() {
    let dir = temp_dir("maintain-json");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["maintain", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["vacuumed"], false);

    std::fs::remove_dir_all(&dir).ok();
}
