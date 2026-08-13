//! Integration tests for `dbs research youtube`/`youtube-backup` (issue
//! #77).
//!
//! Both commands' NotebookLM synthesis step isn't implemented in this
//! port yet (see gap-analysis.md's Research subsystem row), so these
//! tests cover what's real today: flag parsing, default output path
//! (`./<slug>.md`), and `youtube-backup`'s video *selection* against
//! the (empty, in these tests) backup database — including the
//! reference's own "no videos matched" error and its source/list
//! scoping text. The "videos matched" success path is covered by the
//! `dbs-core` unit tests for `BackupService::select_youtube_backup_videos`
//! (which seed a real database), the same split established for
//! `dbs sources`/`dbs connectors` (#71) and `dbs capture` (#76).

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-research-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(dir: &std::path::Path) -> PathBuf {
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "export_dir = \"exports\"").unwrap();
    writeln!(file, "download_root = \"downloads\"").unwrap();
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
fn research_youtube_reports_the_pipeline_stub_and_the_default_out_path() {
    let dir = temp_dir("youtube-stub");
    let config_path = write_config(&dir);

    let output = run(
        &dir,
        &config_path,
        &["research", "youtube", "claude code skills"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("research pipeline isn't implemented"),
        "{stderr}"
    );
    assert!(stderr.contains("claude-code-skills.md"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_respects_a_custom_out_path() {
    let dir = temp_dir("youtube-out");
    let config_path = write_config(&dir);

    let output = run(
        &dir,
        &config_path,
        &[
            "research",
            "youtube",
            "claude code skills",
            "--out",
            "report.md",
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("report.md"), "{stderr}");
    assert!(!stderr.contains("claude-code-skills.md"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_with_no_topic_is_a_usage_error() {
    let dir = temp_dir("youtube-usage");
    let config_path = write_config(&dir);

    let output = run(&dir, &config_path, &["research", "youtube"]);
    assert!(!output.status.success(), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_backup_with_no_sources_configured_reports_no_videos_matched() {
    let dir = temp_dir("backup-empty");
    let config_path = write_config(&dir);

    let output = run(
        &dir,
        &config_path,
        &["research", "youtube-backup", "claude code skills"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("No backed-up YouTube videos matched (any youtube source)"),
        "{stderr}"
    );
    assert!(stderr.contains("dbs backup"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_backup_reports_the_source_scope_in_the_no_match_message() {
    let dir = temp_dir("backup-scoped");
    let config_path = write_config(&dir);

    let output = run(
        &dir,
        &config_path,
        &[
            "research",
            "youtube-backup",
            "claude code skills",
            "--source",
            "my-yt",
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("No backed-up YouTube videos matched (source(s) my-yt)"),
        "{stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_backup_reports_the_list_scope_in_the_no_match_message() {
    let dir = temp_dir("backup-list-scoped");
    let config_path = write_config(&dir);

    let output = run(
        &dir,
        &config_path,
        &[
            "research",
            "youtube-backup",
            "claude code skills",
            "--list",
            "watch-later",
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("list(s) watch-later"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_backup_with_no_topic_is_a_usage_error() {
    let dir = temp_dir("backup-usage");
    let config_path = write_config(&dir);

    let output = run(&dir, &config_path, &["research", "youtube-backup"]);
    assert!(!output.status.success(), "{output:?}");

    std::fs::remove_dir_all(&dir).ok();
}
