//! Real-subprocess integration tests for the `dbs-connector-bluesky`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Bluesky (AT Protocol)
//! API served.
//!
//! Unlike raindrop, `BlueskyConfig::identifier` (the handle/DID) has no
//! non-empty default and `src/main.rs` has no way to inject one today
//! (see its doc comment) — but that turns out not to block a real run:
//! `identifier` is only ever used as an opaque value sent in the
//! `createSession` request body, and nothing in `fetch()` validates it
//! is non-empty before making that HTTP call. A permissive mock that
//! doesn't care what `identifier` it was sent — exactly like a real
//! Bluesky PDS, which authenticates off the app password, not the
//! identifier string's shape — is therefore enough for a full,
//! successful run. So this file uses the same two-test shape as
//! raindrop's, rather than the honest-failure shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

/// `DBS_BLUESKY_TEST_BASE_URL` is process-global, but two tests in this
/// file now set/clear it (`main.rs`'s doc comment's "no other test in
/// this binary" claim held only while there was exactly one). Rust runs
/// `#[test]` functions in parallel by default, so both tests take this
/// lock for their full duration to serialize access instead.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-bluesky"))
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
    assert_eq!(rc.type_, "bluesky");
    assert_eq!(
        rc.handshake.secret_keys,
        vec!["BLUESKY_APP_PASSWORD".to_string()]
    );
    assert!(rc.handshake.item_kinds.contains(&"like".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

fn session_body() -> String {
    serde_json::json!({"accessJwt": "jwt-token", "did": "did:plc:abc123"}).to_string()
}

fn like_record(uri: &str, subject_uri: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "uri": uri,
        "value": {
            "subject": {"uri": subject_uri},
            "createdAt": created_at,
        }
    })
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let _guard = ENV_LOCK.lock().unwrap();
    let mut server = mockito::Server::new();
    let _m_session = server
        .mock(
            "POST",
            mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
        )
        .with_status(200)
        .with_body(session_body())
        .create();

    // Two pages, linked by AT Protocol's opaque cursor param — mirrors
    // the crate's own `pagination_follows_the_returned_cursor_across_pages`
    // unit test. The cursor-less first request is matched by `_m0`
    // (registered first, so — per mockito's LIFO matching — only
    // reached once the cursor-bearing request has already failed to
    // match `_m1`).
    let page0 = serde_json::json!({
        "records": [like_record(
            "at://did:plc:abc123/app.bsky.feed.like/1",
            "at://did:plc:other/app.bsky.feed.post/r1",
            "2024-06-01T00:00:00Z",
        )],
        "cursor": "next-page-cursor",
    });
    let page1 = serde_json::json!({
        "records": [like_record(
            "at://did:plc:abc123/app.bsky.feed.like/2",
            "at://did:plc:other/app.bsky.feed.post/r2",
            "2024-06-02T00:00:00Z",
        )],
    });
    let _m0 = server
        .mock("GET", "/xrpc/com.atproto.repo.listRecords")
        .match_query(mockito::Matcher::Any)
        .with_status(200)
        .with_body(page0.to_string())
        .create();
    let _m1 = server
        .mock("GET", "/xrpc/com.atproto.repo.listRecords")
        .match_query(mockito::Matcher::Regex(
            r"cursor=next-page-cursor".to_string(),
        ))
        .with_status(200)
        .with_body(page1.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_BLUESKY_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock server instead of the live Bluesky API.
    std::env::set_var("DBS_BLUESKY_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("bluesky").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-bluesky", "bluesky", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-bluesky".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([(
            "BLUESKY_APP_PASSWORD".to_string(),
            "app-password".to_string(),
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
    std::env::remove_var("DBS_BLUESKY_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("at://did:plc:abc123/app.bsky.feed.like/1"));
    assert!(live.contains("at://did:plc:abc123/app.bsky.feed.like/2"));
}

/// ADR-0002: proves the *real* per-source config path — `WireRunContext.config`
/// → `Connector::configure` → `self.config.identifier` — works end to end.
/// Unlike `mastodon`/`podcast`, an empty `identifier` doesn't make a run
/// fail today (see this file's module doc comment), so this can't be
/// proven by a run succeeding or failing — instead it asserts the mock
/// `createSession` endpoint actually received the `identifier` the wire
/// config carried, not `BlueskyConfig::default()`'s empty string.
#[test]
fn a_real_run_sends_the_wire_configs_identifier_in_create_session() {
    let _guard = ENV_LOCK.lock().unwrap();
    let mut server = mockito::Server::new();
    let _m_session = server
        .mock(
            "POST",
            mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
        )
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "identifier": "alice.bsky.social",
        })))
        .with_status(200)
        .with_body(session_body())
        .create();
    let _m_records = server
        .mock("GET", "/xrpc/com.atproto.repo.listRecords")
        .match_query(mockito::Matcher::Any)
        .with_status(200)
        .with_body(serde_json::json!({"records": []}).to_string())
        .create();

    std::env::set_var("DBS_BLUESKY_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("bluesky").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-bluesky", "bluesky", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-bluesky".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([(
            "BLUESKY_APP_PASSWORD".to_string(),
            "app-password".to_string(),
        )]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
        config: HashMap::from([(
            "identifier".to_string(),
            serde_json::json!("alice.bsky.social"),
        )]),
        http_timeout: 30.0,
        http_rate_limit_per_min: 0,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, 500, None).unwrap();
    std::env::remove_var("DBS_BLUESKY_TEST_BASE_URL");

    // The point of the test is `_m_session`'s body match above: if
    // `configure()` hadn't applied the wire config's `identifier`, the
    // request would have carried `""` instead and mockito would have
    // returned its default 501 for the unmatched request, surfacing as
    // an error here instead of a clean, empty (zero-item) run.
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 0);
}
