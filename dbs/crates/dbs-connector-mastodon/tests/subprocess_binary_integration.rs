//! Real-subprocess integration tests for the `dbs-connector-mastodon`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Mastodon instance
//! served.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

/// `DBS_MASTODON_TEST_BASE_URL` is process-global, and two tests in
/// this file now set/clear it. Rust runs `#[test]` functions in
/// parallel by default, so both tests take this lock for their full
/// duration to serialize access instead.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-mastodon"))
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
    assert_eq!(rc.type_, "mastodon");
    assert_eq!(rc.handshake.secret_keys, vec!["MASTODON_TOKEN".to_string()]);
    assert!(rc.handshake.item_kinds.contains(&"bookmark".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"favourite".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
    assert!(!rc.handshake.capabilities.supports_incremental);
}

/// Mirrors `status_json` from `src/lib.rs`'s own unit tests — the
/// shape of a single Mastodon status as returned by the
/// `/api/v1/bookmarks` and `/api/v1/favourites` endpoints.
fn status_json(id: &str, acct: &str, content: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "url": format!("https://example.social/@{acct}/{id}"),
        "content": content,
        "created_at": created_at,
        "account": {"acct": acct},
        "tags": [{"name": "rust"}],
        "favourites_count": 3,
    })
}

#[test]
fn a_real_run_against_a_mock_instance_commits_items_through_the_full_subprocess_boundary() {
    let _guard = ENV_LOCK.lock().unwrap();
    let mut server = mockito::Server::new();
    let bookmarks = serde_json::json!([status_json("1", "alice", "hello", "2024-06-01T00:00:00Z")]);
    let favourites = serde_json::json!([status_json("2", "bob", "world", "2024-06-02T00:00:00Z")]);
    let _m_bookmarks = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
        )
        .with_status(200)
        .with_body(bookmarks.to_string())
        .create();
    let _m_favourites = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/api/v1/favourites.*".to_string()),
        )
        .with_status(200)
        .with_body(favourites.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_MASTODON_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock instance instead of a live Mastodon instance.
    std::env::set_var("DBS_MASTODON_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("mastodon").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-mastodon", "mastodon", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-mastodon".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([("MASTODON_TOKEN".to_string(), "secret-token".to_string())]),
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
    std::env::remove_var("DBS_MASTODON_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("bookmark:1"));
    assert!(live.contains("favourite:2"));
}

/// ADR-0002: proves the *real* per-source config path — `WireRunContext.config`
/// → `Connector::configure` → `self.config.instance` — works end to end,
/// deliberately *without* touching `DBS_MASTODON_TEST_BASE_URL` (that env var
/// only exists so a test can point this binary at a mock server; this test
/// instead exercises the production path a real `dbs backup` run takes: the
/// host reading a source's `[sources.NAME]` `instance =` config key and
/// carrying it across the subprocess boundary). Without this, `main.rs`
/// constructs `MastodonConfig::default()` (`instance: ""`), and `fetch`
/// would reject it before ever reaching the mock server below.
#[test]
fn a_real_run_gets_its_instance_from_wire_config_with_no_test_env_var_set() {
    let _guard = ENV_LOCK.lock().unwrap();
    let mut server = mockito::Server::new();
    let bookmarks = serde_json::json!([status_json("1", "alice", "hello", "2024-06-01T00:00:00Z")]);
    let favourites = serde_json::json!([]);
    let _m_bookmarks = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
        )
        .with_status(200)
        .with_body(bookmarks.to_string())
        .create();
    let _m_favourites = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/api/v1/favourites.*".to_string()),
        )
        .with_status(200)
        .with_body(favourites.to_string())
        .create();

    // Deliberately not set — this is the point of the test.
    std::env::remove_var("DBS_MASTODON_TEST_BASE_URL");

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("mastodon").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-mastodon", "mastodon", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-mastodon".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([("MASTODON_TOKEN".to_string(), "secret-token".to_string())]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
        config: HashMap::from([("instance".to_string(), serde_json::json!(server.url()))]),
        http_timeout: 30.0,
        http_rate_limit_per_min: 0,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, 500, None).unwrap();

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 1);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("bookmark:1"));
}
