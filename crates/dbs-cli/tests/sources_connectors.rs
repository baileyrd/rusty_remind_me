//! Integration tests for `dbs sources` and `dbs connectors` (issue #71).
//!
//! The CLI always constructs an empty connector registry (no
//! connector-candidate discovery exists yet, #85-100), so the
//! "connector found"/"source added successfully" paths can't be
//! exercised through the compiled binary — see the `dbs-core` unit
//! tests in `service.rs` (which build a real registry entry via
//! `ConnectorRegistry::from_resolved`) for that coverage. These tests
//! cover what the CLI *can* honestly produce today: empty state,
//! populated config, and a connector `check`/`describe`/`add` failure
//! surfaced to CLI output — exactly the acceptance checklist's scope.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-sources-connectors-test-{label}-{}",
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
fn sources_list_with_no_sources_configured_prompts_to_add_one() {
    let dir = temp_dir("sources-list-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["sources", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("dbs sources add"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_list_reports_every_configured_source() {
    let dir = temp_dir("sources-list-multi");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = false\n",
    );

    let output = run(&dir, &config_path, &["sources", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("a") && stdout.contains("enabled"),
        "{stdout}"
    );
    assert!(
        stdout.contains("b") && stdout.contains("disabled"),
        "{stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_list_json_emits_a_valid_json_array() {
    let dir = temp_dir("sources-list-json");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(&dir, &config_path, &["sources", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed[0]["name"], "a");
    assert_eq!(parsed[0]["backed_up"], false);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_check_surfaces_a_connector_not_found_failure() {
    let dir = temp_dir("sources-check-fail");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(&dir, &config_path, &["sources", "check"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("a:"), "{stdout}");
    assert!(stdout.contains("not found"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_check_with_no_sources_exits_zero() {
    let dir = temp_dir("sources-check-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["sources", "check"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_add_fails_for_an_unregistered_connector_type_and_does_not_touch_the_file() {
    let dir = temp_dir("sources-add-fail");
    let config_path = write_config(&dir, "");
    let before = std::fs::read_to_string(&config_path).unwrap();

    let output = run(
        &dir,
        &config_path,
        &["sources", "add", "a", "--type", "raindrop"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not found"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_add_with_a_malformed_set_pair_is_a_usage_error() {
    let dir = temp_dir("sources-add-bad-set");
    let config_path = write_config(&dir, "");

    let output = run(
        &dir,
        &config_path,
        &[
            "sources",
            "add",
            "a",
            "--type",
            "raindrop",
            "--set",
            "no-equals-sign",
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("key=value"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_add_rejects_a_name_that_already_exists() {
    let dir = temp_dir("sources-add-dup");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = run(
        &dir,
        &config_path,
        &["sources", "add", "a", "--type", "raindrop"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("already exists"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn connectors_list_with_no_connectors_registered_prints_nothing() {
    let dir = temp_dir("connectors-list-empty");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["connectors", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.trim().is_empty(), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn connectors_list_json_emits_an_empty_array() {
    let dir = temp_dir("connectors-list-json");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["connectors", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn connectors_describe_an_unregistered_type_is_a_config_error() {
    let dir = temp_dir("connectors-describe-fail");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["connectors", "describe", "raindrop"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not found"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}
