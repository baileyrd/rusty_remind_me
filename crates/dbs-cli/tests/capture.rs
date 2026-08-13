//! Integration tests for `dbs capture` (issue #76).
//!
//! The CLI always constructs an empty connector registry (no
//! connector-candidate discovery exists yet, #85-100), so a target that
//! resolves to a real connector with an `auth_capture` spec can't be
//! exercised through the compiled binary — see the `dbs-core` unit
//! tests in `service.rs` (`resolve_capture_target_*`, which build a
//! real registry entry via `ConnectorRegistry::from_resolved`) for that
//! coverage. These tests cover what the CLI *can* honestly produce
//! today: target-resolution failures (no such connector/source, or a
//! configured source whose connector type isn't registered) and flag
//! parsing — the actual browser capture isn't implemented in this port
//! (gap-analysis.md's Connectors cluster rows).

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-capture-test-{label}-{}",
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
fn capture_an_unknown_connector_or_source_is_a_config_error() {
    let dir = temp_dir("unknown");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["capture", "nope"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no such connector or source"), "{stderr}");
    assert!(stderr.contains("\"nope\""), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn capture_falls_back_to_a_configured_source_name_and_reports_its_unregistered_type() {
    let dir = temp_dir("source-fallback");
    let config_path = write_config(
        &dir,
        "[sources.myrd]\ntype = \"raindrop\"\nenabled = true\n",
    );

    let output = run(&dir, &config_path, &["capture", "myrd"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not found"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn capture_accepts_an_out_flag_without_changing_the_resolution_failure() {
    let dir = temp_dir("out-flag");
    let config_path = write_config(&dir, "");

    let output = run(
        &dir,
        &config_path,
        &["capture", "nope", "--out", "somewhere.zip"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no such connector or source"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn capture_with_no_target_is_a_usage_error() {
    let dir = temp_dir("missing-target");
    let config_path = write_config(&dir, "");

    let output = run(&dir, &config_path, &["capture"]);
    assert!(!output.status.success(), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}
