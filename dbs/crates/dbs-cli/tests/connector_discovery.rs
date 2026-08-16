//! Integration tests for issue #160: real `dbs-connector-*` candidate
//! discovery. `sources_connectors.rs`'s tests cover the honest
//! always-empty-registry behavior that predates this issue (`PATH`
//! never has any `dbs-connector-*` binaries in these tests' sandboxed
//! environment, so those assertions still hold); these tests instead
//! point `[dbs] connectors_dir` at a directory holding a real,
//! compiled `dbs-connector-raindrop` binary and confirm the CLI
//! actually finds and handshakes with it.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

/// `CARGO_BIN_EXE_<name>` is only set for the current *package's own*
/// binary targets, not a dev-dependency's — so this locates the real,
/// already-compiled `dbs-connector-raindrop` binary the way Cargo
/// itself lays workspace binaries out: as a sibling of this very test
/// binary, two directories up (`target/<profile>/deps/<test> ->
/// target/<profile>/`). Building it in the first place only happens
/// because `dbs-connector-raindrop` is a dev-dependency of this crate
/// (see `Cargo.toml`) — Cargo compiles a path dependency's binary
/// targets even though it doesn't expose their `CARGO_BIN_EXE_*` vars.
fn raindrop_connector_bin() -> PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    let profile_dir = test_exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test exe is under target/<profile>/deps/");
    profile_dir.join(format!(
        "dbs-connector-raindrop{}",
        std::env::consts::EXE_SUFFIX
    ))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-connector-discovery-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// A directory containing nothing but a copy of the real, compiled
/// `dbs-connector-raindrop` binary — what `connectors_dir` points at.
fn connectors_dir_with_raindrop(label: &str) -> PathBuf {
    let dir = temp_dir(label).join("connectors");
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("dbs-connector-raindrop");
    std::fs::copy(raindrop_connector_bin(), &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn write_config(
    dir: &std::path::Path,
    connectors_dir: &std::path::Path,
    sources_block: &str,
) -> PathBuf {
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "export_dir = \"exports\"").unwrap();
    writeln!(file, "download_root = \"downloads\"").unwrap();
    writeln!(
        file,
        "connectors_dir = {:?}",
        connectors_dir.to_string_lossy()
    )
    .unwrap();
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
fn connectors_describe_finds_a_real_connector_via_connectors_dir() {
    let dir = temp_dir("describe-found");
    let connectors_dir = connectors_dir_with_raindrop("describe-found");
    let config_path = write_config(&dir, &connectors_dir, "");

    let output = run(&dir, &config_path, &["connectors", "describe", "raindrop"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Raindrop.io"), "{stdout}");
    assert!(stdout.contains("RAINDROP_TOKEN"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn connectors_list_reports_a_connector_found_via_connectors_dir() {
    let dir = temp_dir("list-found");
    let connectors_dir = connectors_dir_with_raindrop("list-found");
    let config_path = write_config(&dir, &connectors_dir, "");

    let output = run(&dir, &config_path, &["connectors", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("raindrop"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_check_succeeds_once_the_connector_is_actually_discoverable() {
    let dir = temp_dir("check-found");
    let connectors_dir = connectors_dir_with_raindrop("check-found");
    let config_path = write_config(
        &dir,
        &connectors_dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n",
    );

    let output = run(&dir, &config_path, &["sources", "check"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("a:"), "{stdout}");
    assert!(!stdout.contains("not found"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn discovery_still_finds_nothing_without_a_configured_connectors_dir() {
    // Same source config as the "found" tests above, minus
    // connectors_dir — confirms PATH-only discovery (this sandboxed
    // test process's PATH has no dbs-connector-* binaries on it)
    // still behaves exactly like the pre-#160 always-empty registry.
    let dir = temp_dir("check-not-found");
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "[sources.a]\ntype = \"raindrop\"\nenabled = true").unwrap();
    drop(file);

    let output = run(&dir, &config_path, &["sources", "check"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("not found"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
