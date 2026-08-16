//! Integration tests for `dbs items` and `dbs stats` (issue #69).
//!
//! Uses a real `SqliteStorage` (same crate `dbs-cli` already depends on)
//! to seed genuine item rows directly — no connector-candidate discovery
//! exists yet (#85-100), so a real `dbs backup` run can never produce
//! this data itself.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use dbs_core::{PreparedItem, SqliteStorage, Storage};

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-items-stats-test-{label}-{}",
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

fn prepared(external_id: &str, title: &str, body: &str) -> PreparedItem {
    PreparedItem {
        external_id: external_id.to_string(),
        item_kind: "bookmark".to_string(),
        title: Some(title.to_string()),
        url: Some(format!("https://example.com/{external_id}")),
        body: Some(body.to_string()),
        tags: vec!["rust".to_string()],
        item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
        item_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        content_hash: format!("hash-{external_id}"),
        raw_json: "{}".to_string(),
        deleted: false,
        media: Vec::new(),
    }
}

/// Seeds `source_name` with the given items via a real `upsert_items`
/// call — the only way this DB shape gets built without a real
/// connector.
fn seed_items(dir: &std::path::Path, source_name: &str, items: &[PreparedItem]) -> i64 {
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
    source.id
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
fn items_with_an_empty_database_reports_no_matches() {
    let dir = temp_dir("items-empty");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(&dir, &config_path, &["items"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), "No items matched.");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_lists_seeded_items() {
    let dir = temp_dir("items-multi");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(
        &dir,
        "a",
        &[
            prepared("1", "First Item", "hello world"),
            prepared("2", "Second Item", "goodbye world"),
        ],
    );

    let output = run(&dir, &config_path, &["items"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("First Item"), "{stdout}");
    assert!(stdout.contains("Second Item"), "{stdout}");
    assert!(stdout.contains("1-2 of 2"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_search_filters_by_title_and_body() {
    let dir = temp_dir("items-search");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(
        &dir,
        "a",
        &[
            prepared("1", "Rust Programming", "systems language"),
            prepared("2", "Python Notes", "scripting language"),
        ],
    );

    let output = run(&dir, &config_path, &["items", "--search", "rust"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Rust Programming"), "{stdout}");
    assert!(!stdout.contains("Python Notes"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_source_filter_narrows_the_listing() {
    let dir = temp_dir("items-source-filter");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = true\n",
    );
    seed_items(&dir, "a", &[prepared("1", "From A", "body")]);
    seed_items(&dir, "b", &[prepared("2", "From B", "body")]);

    let output = run(&dir, &config_path, &["items", "--source", "a"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("From A"), "{stdout}");
    assert!(!stdout.contains("From B"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_limit_flag_paginates() {
    let dir = temp_dir("items-limit");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(
        &dir,
        "a",
        &[
            prepared("1", "Item One", "body"),
            prepared("2", "Item Two", "body"),
            prepared("3", "Item Three", "body"),
        ],
    );

    let output = run(&dir, &config_path, &["items", "-n", "2"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("1-2 of 3"), "{stdout}");
    assert!(stdout.contains("next page: --offset 2"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_json_emits_the_web_ui_envelope() {
    let dir = temp_dir("items-json");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body")]);

    let output = run(&dir, &config_path, &["items", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["total"], 1);
    assert_eq!(parsed["items"][0]["title"], "Item One");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_by_id_shows_full_detail() {
    let dir = temp_dir("items-detail");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "the body text")]);

    let list_output = run(&dir, &config_path, &["items", "--json"]);
    let list_stdout = String::from_utf8(list_output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&list_stdout).unwrap();
    let id = parsed["items"][0]["id"].as_i64().unwrap();

    let output = run(&dir, &config_path, &["items", &id.to_string()]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Item One"), "{stdout}");
    assert!(stdout.contains("the body text"), "{stdout}");
    assert!(stdout.contains("raw:"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_by_unknown_id_exits_1() {
    let dir = temp_dir("items-unknown-id");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["items", "999"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no such item 999"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn items_invalid_date_is_a_usage_error() {
    let dir = temp_dir("items-bad-date");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["items", "--since", "not-a-date"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Invalid date"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stats_with_an_empty_database_says_so() {
    let dir = temp_dir("stats-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("0 live, 0 deleted (0 total)"), "{stdout}");
    assert!(stdout.contains("No items stored yet"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stats_aggregates_seeded_items_by_source_and_kind() {
    let dir = temp_dir("stats-seeded");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(
        &dir,
        "a",
        &[
            prepared("1", "Item One", "body"),
            prepared("2", "Item Two", "body"),
        ],
    );

    let output = run(&dir, &config_path, &["stats"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("2 live, 0 deleted (2 total)"), "{stdout}");
    assert!(
        stdout.contains("a") && stdout.contains("bookmark"),
        "{stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stats_json_emits_the_raw_metrics_shape() {
    let dir = temp_dir("stats-json");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body")]);

    let output = run(&dir, &config_path, &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["by_source_kind"][0]["source"], "a");
    assert_eq!(parsed["by_source_kind"][0]["live"], 1);

    std::fs::remove_dir_all(&dir).ok();
}
