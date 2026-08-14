//! Integration tests for `dbs research youtube`/`youtube-backup` (issue
//! #77, wired to the real `dbs-research` pipeline in #189).
//!
//! Both commands' NotebookLM synthesis step is real but can never
//! succeed against `notebooklm::UnimplementedClient` (see that type's
//! own doc-comment — Decision 4's real adapter is deferred pending
//! #84), so these tests cover what's actually verifiable in a sandbox
//! with no live network access: flag parsing, default output path
//! (`./<slug>.md`), no report file written on failure,
//! `youtube-backup`'s video *selection* against the (empty, in most of
//! these tests) backup database — including the reference's own "no
//! videos matched" error and its source/list scoping text — and, with
//! a real seeded video, that selection succeeding and the pipeline
//! actually running past it before failing at the NotebookLM step.

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

// `research youtube` now runs the real pipeline (issue #189):
// `dbs_research::pipeline::run_pipeline` shells out to a real `yt-dlp`
// for search, then hits `notebooklm::UnimplementedClient`. Neither
// step can succeed in this test environment (`yt-dlp` may or may not
// be on `PATH`, and even if it is, this sandbox has no live network
// access to youtube.com) — so rather than pin the *specific* failure
// message (which depends on which of those two unavailable
// dependencies it hits first), these tests only assert the two things
// guaranteed regardless: a non-zero exit and no report file written
// (the report is only written on success).

#[test]
fn research_youtube_fails_cleanly_and_writes_no_report() {
    let dir = temp_dir("youtube-fail");
    let config_path = write_config(&dir);

    let output = run(
        &dir,
        &config_path,
        &["research", "youtube", "claude code skills"],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.is_empty(), "{stderr}");
    assert!(!dir.join("claude-code-skills.md").exists());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn research_youtube_with_a_custom_out_path_still_writes_nothing_on_failure() {
    let dir = temp_dir("youtube-out-fail");
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
    assert!(!dir.join("report.md").exists());
    assert!(!dir.join("claude-code-skills.md").exists());

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

/// With a real backed-up video seeded, selection succeeds — the
/// pipeline now actually runs (past the "no videos matched" early
/// return) and only fails at the NotebookLM step.
#[test]
fn research_youtube_backup_with_a_matched_video_runs_the_pipeline_and_fails_cleanly() {
    let dir = temp_dir("backup-real-video");
    let config_path = write_config(&dir);

    {
        use dbs_core::{PreparedItem, SqliteStorage, Storage};
        let db_path = dir.join("dbs.sqlite3");
        let mut storage = SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();
        let source = storage
            .upsert_source("yt", "youtube", "p", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let item = PreparedItem {
            external_id: "v1".to_string(),
            item_kind: "video".to_string(),
            title: Some("A great video".to_string()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string()),
            body: None,
            tags: vec![],
            item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            item_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            content_hash: "h1".to_string(),
            raw_json: serde_json::json!({
                "id": "dQw4w9WgXcQ",
                "channel": "A Channel",
                "view_count": 12345,
            })
            .to_string(),
            deleted: false,
            media: Vec::new(),
        };
        storage
            .upsert_items(source.id, run_id, &[item], true, 0)
            .unwrap();
    }

    let output = run(
        &dir,
        &config_path,
        &[
            "research",
            "youtube-backup",
            "claude code skills",
            "--source",
            "yt",
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("No backed-up YouTube videos matched"),
        "{stderr}"
    );
    assert!(!stderr.is_empty(), "{stderr}");
    assert!(!dir.join("claude-code-skills.md").exists());

    std::fs::remove_dir_all(&dir).ok();
}
