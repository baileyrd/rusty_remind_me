//! Real-subprocess integration tests for the `dbs-connector-vimeo`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Vimeo API served.
//!
//! `download_videos` is `false` by default (see `VimeoConfig`), so
//! this exercises the plain REST+bearer-token catalog path only — the
//! same boundary as raindrop's own subprocess test. The `yt-dlp`
//! download path is unreachable with default config and is already
//! covered by `dbs-connector-vimeo`'s own unit tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-vimeo"))
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
    assert_eq!(rc.type_, "vimeo");
    assert_eq!(rc.handshake.secret_keys, vec!["VIMEO_TOKEN".to_string()]);
    assert!(rc.handshake.item_kinds.contains(&"video".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

/// Same shape as `src/lib.rs`'s own `video_json` test fixture: a
/// single-page `GET /me/videos` response with one video.
fn vimeo_json(id: &str, name: &str, link: &str) -> serde_json::Value {
    serde_json::json!({
        "uri": format!("/videos/{id}"),
        "name": name,
        "link": link,
        "description": "a video",
        "created_time": "2024-06-01T00:00:00Z",
        "modified_time": "2024-06-02T00:00:00Z",
        "pictures": {"base_link": format!("https://i.vimeocdn.com/{id}.jpg")},
        "tags": [{"name": "rust"}],
    })
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();
    let body = serde_json::json!({
        "data": [vimeo_json("1", "A Video", "https://vimeo.com/1")],
        "paging": {"next": null},
    });
    let _m = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/me/videos\?.*".to_string()),
        )
        .with_status(200)
        .with_body(body.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_VIMEO_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock server instead of the live Vimeo API.
    std::env::set_var("DBS_VIMEO_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("vimeo").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-vimeo", "vimeo", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-vimeo".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([("VIMEO_TOKEN".to_string(), "secret-token".to_string())]),
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
    std::env::remove_var("DBS_VIMEO_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 1);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("1"));
}
