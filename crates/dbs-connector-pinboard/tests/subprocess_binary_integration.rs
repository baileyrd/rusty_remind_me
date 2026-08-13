//! Real-subprocess integration tests for the `dbs-connector-pinboard`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Pinboard API served.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-pinboard"))
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
    assert_eq!(rc.type_, "pinboard");
    assert_eq!(rc.handshake.secret_keys, vec!["PINBOARD_TOKEN".to_string()]);
    assert!(rc.handshake.item_kinds.contains(&"bookmark".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

fn post_json(
    hash: &str,
    description: &str,
    href: &str,
    time: &str,
    tags: &str,
) -> serde_json::Value {
    serde_json::json!({
        "hash": hash,
        "description": description,
        "href": href,
        "time": time,
        "tags": tags,
        "extended": "notes",
    })
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();
    // `fetch()` always checks `posts/update` first — see lib.rs's
    // doc comment on the connector: it's the cheap global
    // change-signal Pinboard offers, checked before ever paging
    // `posts/all`.
    let _m_update = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
        )
        .with_status(200)
        .with_body(r#"{"update_time": "2024-06-01T00:00:00Z"}"#)
        .create();
    let posts = serde_json::json!([
        post_json(
            "1",
            "First",
            "https://example.com/1",
            "2024-06-01T00:00:00Z",
            "a b"
        ),
        post_json(
            "2",
            "Second",
            "https://example.com/2",
            "2024-05-01T00:00:00Z",
            "c"
        ),
    ]);
    let _m_all = server
        .mock("GET", mockito::Matcher::Regex(r"^/posts/all.*".to_string()))
        .with_status(200)
        .with_body(posts.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_PINBOARD_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock server instead of the live Pinboard API.
    std::env::set_var("DBS_PINBOARD_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("pinboard").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-pinboard", "pinboard", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-pinboard".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([(
            "PINBOARD_TOKEN".to_string(),
            "user:secret-token".to_string(),
        )]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, None).unwrap();
    std::env::remove_var("DBS_PINBOARD_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("1"));
    assert!(live.contains("2"));
}
