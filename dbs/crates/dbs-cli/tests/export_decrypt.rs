//! Integration tests for `dbs export*` and `dbs decrypt` (issue #70).
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
        "dbs-cli-export-decrypt-test-{label}-{}",
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

#[test]
fn export_ndjson_writes_seeded_items_to_a_file() {
    let dir = temp_dir("ndjson");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body text")]);

    let output = run(
        &dir,
        &config_path,
        &["export", "--out", "out.ndjson", "--format", "ndjson"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Exported 1 item(s)"), "{stdout}");
    let content = std::fs::read_to_string(dir.join("out.ndjson")).unwrap();
    assert!(content.contains("\"external_id\":\"1\""), "{content}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_json_writes_seeded_items_to_a_file() {
    let dir = temp_dir("json");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body text")]);

    let output = run(
        &dir,
        &config_path,
        &["export", "--out", "out.json", "--format", "json"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let content = std::fs::read_to_string(dir.join("out.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(parsed.is_array() || parsed.is_object(), "{content}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_csv_writes_seeded_items_to_a_file() {
    let dir = temp_dir("csv");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body text")]);

    let output = run(
        &dir,
        &config_path,
        &["export", "--out", "out.csv", "--format", "csv"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let content = std::fs::read_to_string(dir.join("out.csv")).unwrap();
    assert!(content.contains("Item One"), "{content}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_archive_writes_a_zip_bundle() {
    let dir = temp_dir("archive");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body text")]);

    let output = run(
        &dir,
        &config_path,
        &["export", "--out", "out.zip", "--format", "archive"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(dir.join("out.zip").is_file());
    let magic = std::fs::read(dir.join("out.zip")).unwrap();
    assert_eq!(&magic[..2], b"PK", "not a zip file: {magic:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_unknown_format_is_a_config_error() {
    let dir = temp_dir("bad-format");
    let config_path = write_config(&dir, "");

    let output = run(
        &dir,
        &config_path,
        &["export", "--out", "out.bin", "--format", "bogus"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("bogus"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_encrypt_then_decrypt_round_trips_to_the_plain_content() {
    let dir = temp_dir("roundtrip");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "the body text")]);

    let plain = run(
        &dir,
        &config_path,
        &["export", "--out", "plain.ndjson", "--format", "ndjson"],
    );
    assert_eq!(plain.status.code(), Some(0), "{plain:?}");

    let encrypted = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "export",
            "--out",
            "secret.ndjson.enc",
            "--format",
            "ndjson",
            "--encrypt",
        ])
        .env("DBS_EXPORT_PASSPHRASE", "hunter2")
        .output()
        .unwrap();
    assert_eq!(encrypted.status.code(), Some(0), "{encrypted:?}");
    // The file is genuinely encrypted, not plaintext with a different name.
    let enc_bytes = std::fs::read(dir.join("secret.ndjson.enc")).unwrap();
    assert_ne!(enc_bytes, std::fs::read(dir.join("plain.ndjson")).unwrap());

    let decrypted = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .args(["decrypt", "secret.ndjson.enc"])
        .env("DBS_EXPORT_PASSPHRASE", "hunter2")
        .output()
        .unwrap();
    assert_eq!(decrypted.status.code(), Some(0), "{decrypted:?}");

    // Each export run serializes each row's HashMap in its own random
    // key order, so compare parsed JSON values (order-independent),
    // not raw text.
    let plain_content = std::fs::read_to_string(dir.join("plain.ndjson")).unwrap();
    let roundtrip_content = std::fs::read_to_string(dir.join("secret.ndjson")).unwrap();
    let plain_value: serde_json::Value = serde_json::from_str(plain_content.trim()).unwrap();
    let roundtrip_value: serde_json::Value =
        serde_json::from_str(roundtrip_content.trim()).unwrap();
    assert_eq!(plain_value, roundtrip_value);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn decrypt_refuses_to_overwrite_an_existing_destination() {
    let dir = temp_dir("decrypt-no-clobber");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body")]);

    Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "export",
            "--out",
            "secret.ndjson.enc",
            "--format",
            "ndjson",
            "--encrypt",
        ])
        .env("DBS_EXPORT_PASSPHRASE", "hunter2")
        .output()
        .unwrap();
    std::fs::write(dir.join("secret.ndjson"), "already here").unwrap();

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .args(["decrypt", "secret.ndjson.enc"])
        .env("DBS_EXPORT_PASSPHRASE", "hunter2")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("secret.ndjson")).unwrap(),
        "already here"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn decrypt_a_plain_file_is_a_config_error() {
    let dir = temp_dir("decrypt-not-encrypted");
    let config_path = write_config(&dir, "");
    std::fs::write(dir.join("plain.ndjson"), "not encrypted").unwrap();

    let output = run(&dir, &config_path, &["decrypt", "plain.ndjson"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not a dbs-encrypted file"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn decrypt_a_missing_file_is_a_config_error() {
    let dir = temp_dir("decrypt-missing");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["decrypt", "nonexistent.enc"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no such file"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn decrypt_with_the_wrong_passphrase_fails_and_cleans_up() {
    let dir = temp_dir("decrypt-wrong-pass");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "body")]);

    Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "export",
            "--out",
            "secret.ndjson.enc",
            "--format",
            "ndjson",
            "--encrypt",
        ])
        .env("DBS_EXPORT_PASSPHRASE", "hunter2")
        .output()
        .unwrap();

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .args(["decrypt", "secret.ndjson.enc"])
        .env("DBS_EXPORT_PASSPHRASE", "wrong-passphrase")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    // Failed decrypt must not leave a partial destination file behind.
    assert!(!dir.join("secret.ndjson").exists());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_notes_writes_one_markdown_file_per_item() {
    let dir = temp_dir("notes");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "note body")]);

    let output = run(&dir, &config_path, &["export-notes", "--out-dir", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Wrote 1 note(s)"), "{stdout}");
    let md_files: Vec<_> = std::fs::read_dir(dir.join("notes"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();
    assert_eq!(md_files.len(), 1, "{md_files:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_wiki_writes_pages_and_an_index() {
    let dir = temp_dir("wiki");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");
    seed_items(&dir, "a", &[prepared("1", "Item One", "wiki body")]);

    let output = run(&dir, &config_path, &["export-wiki", "--out-dir", "wiki"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(dir.join("wiki").join("index.md").is_file());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_profiles_marks_a_source_level_override() {
    let dir = temp_dir("profiles");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.a.export]\nenabled = false\n",
    );

    let output = run(&dir, &config_path, &["export-profiles"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("EXCLUDED"), "{stdout}");
    assert!(!stdout.contains("* item kinds"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_profiles_json_reports_the_override_field_list() {
    let dir = temp_dir("profiles-json");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.a.export]\nenabled = false\n",
    );

    let output = run(&dir, &config_path, &["export-profiles", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["a"]["overridden"], serde_json::json!(["enabled"]));
    assert_eq!(parsed["a"]["resolved"]["enabled"], false);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_profiles_with_no_sources_says_so() {
    let dir = temp_dir("profiles-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["export-profiles"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), "No sources configured.");

    std::fs::remove_dir_all(&dir).ok();
}
