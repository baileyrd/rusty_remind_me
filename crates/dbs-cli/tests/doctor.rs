//! Integration tests for `dbs doctor` (issue #72).
//!
//! The CLI always constructs an empty connector registry (no
//! connector-candidate discovery exists yet, #85-100), so an enabled
//! source can only ever surface the "connector unavailable" failure
//! here — the ok-path secrets/VPN/staleness checks are covered at the
//! `dbs-core` unit-test level in `service.rs` (which builds a real
//! registry entry via `ConnectorRegistry::from_resolved`).

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-doctor-test-{label}-{}",
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
fn doctor_with_no_sources_reports_only_database_checks_and_exits_zero() {
    let dir = temp_dir("no-sources");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("database.integrity"), "{stdout}");
    assert!(stdout.contains("database.wal"), "{stdout}");
    assert!(stdout.contains("runs.interrupted"), "{stdout}");
    assert!(!stdout.contains("source."), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_reports_an_unresolvable_source_connector_and_exits_one() {
    let dir = temp_dir("bad-connector");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(&dir, &config_path, &["doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[fail]"), "{stdout}");
    assert!(stdout.contains("source.a"), "{stdout}");
    assert!(stdout.contains("unavailable"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_marks_a_disabled_source_ok_and_does_not_fail_the_run() {
    let dir = temp_dir("disabled-source");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = false\n");

    let output = run(&dir, &config_path, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[ ok ] source.a: disabled"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_json_emits_a_valid_json_array() {
    let dir = temp_dir("json");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = parsed.as_array().unwrap();
    assert!(
        arr.iter().any(|c| c["name"] == "database.integrity"),
        "{stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn doctor_json_still_exits_one_on_a_failing_check() {
    let dir = temp_dir("json-fail");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(&dir, &config_path, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = parsed.as_array().unwrap();
    assert!(arr.iter().any(|c| c["status"] == "fail"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
