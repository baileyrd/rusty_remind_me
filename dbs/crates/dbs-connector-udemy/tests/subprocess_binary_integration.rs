//! Real-subprocess integration tests for the `dbs-connector-udemy`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Udemy API served.
//!
//! `UdemyConfig::default()` has `download_videos: false`, so the
//! real run below is plain REST — no `yt-dlp` subprocess involved —
//! and the mock setup mirrors `src/lib.rs`'s own
//! `full_fetch_yields_course_lecture_and_quiz_items_and_a_reconcile_marker`
//! test exactly (same endpoints, same fixture shapes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-udemy"))
}

fn candidate() -> ConnectorCandidate {
    ConnectorCandidate {
        dist_name: "rusty_dbs".to_string(),
        is_builtin: true,
        command: binary_path(),
        args: Vec::new(),
    }
}

#[test]
fn the_real_binarys_handshake_is_valid_and_matches_the_connector() {
    let mut registry = ConnectorRegistry::new();
    let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.loaded.len(), 1);
    let rc = &report.loaded[0];
    assert_eq!(rc.type_, "udemy");
    assert_eq!(
        rc.handshake.secret_keys,
        vec![
            "UDEMY_ACCESS_TOKEN".to_string(),
            "UDEMY_COOKIES_FILE".to_string()
        ]
    );
    assert!(rc.handshake.item_kinds.contains(&"course".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"lecture".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"quiz".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
    assert!(rc.handshake.capabilities.requires_auth);
}

fn course_json(id: i64, title: &str, slug: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "title": title,
        "url": format!("/course/{slug}/"),
        "image_480x270": format!("https://img.udemycdn.com/{id}.jpg"),
        "published_title": slug,
    })
}

fn results_page(items: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({"results": items, "next": null})
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();

    // One enrolled course, matching src/lib.rs's own full-fetch test
    // fixture shapes exactly.
    let courses = results_page(vec![course_json(1, "Rust 101", "rust-101")]);
    let _m_courses = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
        )
        .with_status(200)
        .with_body(courses.to_string())
        .create();

    // That course's curriculum: a chapter heading, one video lecture,
    // and one quiz — exercising all three item kinds in one run.
    let curriculum = results_page(vec![
        serde_json::json!({"_class": "chapter", "title": "Chapter One"}),
        serde_json::json!({
            "_class": "lecture",
            "id": 10,
            "title": "Intro",
            "object_index": 1,
            "asset": {"asset_type": "Video"},
        }),
        serde_json::json!({
            "_class": "quiz",
            "id": 11,
            "title": "Quiz One",
            "object_index": 2,
        }),
    ]);
    let _m_curr = server
        .mock(
            "GET",
            mockito::Matcher::Regex(
                r"^/api-2\.0/courses/1/subscriber-curriculum-items/\?.*".to_string(),
            ),
        )
        .with_status(200)
        .with_body(curriculum.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no
    // other test in this binary reads or writes
    // DBS_UDEMY_TEST_BASE_URL. `std::process::Command` (used by
    // `run_connector_subprocess`) inherits it, which is how the real
    // spawned binary is pointed at the mock server instead of the
    // live Udemy API.
    std::env::set_var("DBS_UDEMY_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("udemy").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-udemy", "udemy", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-udemy".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        // Only UDEMY_ACCESS_TOKEN is actually read by `fetch()` in
        // this run: `download_videos` is off by default, so the
        // UDEMY_COOKIES_FILE-gated yt-dlp path is never reached and
        // doesn't need a value here.
        secrets: HashMap::from([(
            "UDEMY_ACCESS_TOKEN".to_string(),
            "secret-access-token".to_string(),
        )]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
        config: HashMap::new(),
        http_timeout: 30.0,
        http_rate_limit_per_min: 0,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, 500, None).unwrap();
    std::env::remove_var("DBS_UDEMY_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    // course + lecture + quiz.
    assert_eq!(outcome.items_seen, 3);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("course:1"));
    assert!(live.contains("lecture:1:10"));
    // Quizzes share the "lecture:" identity prefix per the reference.
    assert!(live.contains("lecture:1:11"));
}
