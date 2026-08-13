//! Integration tests for `dbs backup --all --parallel N` (issue #66).
//!
//! The worker-pool mechanics themselves (concurrent execution, failure
//! isolation, the in-memory-database fallback) are covered at the
//! `BackupService` unit-test level in `dbs-core` with a real, file-backed
//! `SqliteStorage` — the only backend `Storage::spawn` can serve workers
//! from, and the only place a registered fake connector lets a run
//! actually succeed. The CLI always constructs an empty registry (no
//! connector-candidate discovery exists yet, #85-100), so every enabled
//! source here fails the same way regardless of `--parallel` — these
//! tests just confirm the flag is wired end to end and doesn't change
//! *which* sources run, only how.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-backup-parallel-test-{label}-{}",
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

#[test]
fn parallel_n_runs_every_source() {
    let dir = temp_dir("multi");
    let config_path = write_config(
        &dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.b]\ntype = \"raindrop\"\nenabled = true\n\
         [sources.c]\ntype = \"raindrop\"\nenabled = true\n",
    );

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--parallel")
        .arg("3")
        .output()
        .unwrap();
    // Every source fails (no connector registered) — but all three
    // still ran and are reported, which is what `--parallel` wiring
    // can be checked against without a real connector.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for name in ["a", "b", "c"] {
        assert!(stdout.contains(&format!("  {name} ")), "{stdout}");
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn parallel_one_behaves_like_the_sequential_path() {
    let dir = temp_dir("one");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .arg("--parallel")
        .arg("1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  a "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn without_parallel_the_config_default_still_runs_sequentially() {
    let dir = temp_dir("default");
    let config_path = write_config(&dir, "[sources.a]\ntype = \"raindrop\"\nenabled = true\n");

    let output = Command::new(dbs_bin())
        .current_dir(&dir)
        .arg("--config")
        .arg(&config_path)
        .arg("backup")
        .arg("--all")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("  a "), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
