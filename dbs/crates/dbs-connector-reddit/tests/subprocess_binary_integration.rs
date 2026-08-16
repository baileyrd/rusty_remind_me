//! Real-subprocess integration tests for the `dbs-connector-reddit`
//! binary (issue #164) — proves ADR-0001's protocol end to end for
//! this connector: spawns the actual compiled binary, drives it
//! through `dbs_core::registry`'s handshake discovery (#45) and
//! `dbs_core::run_stream`'s run/stream bridge (#157) exactly the way
//! `dbs-cli` would.
//!
//! Unlike `dbs-connector-raindrop`'s equivalent test file, there is no
//! "a real run against a mock API commits items" test here: since
//! #187, `fetch()` really does shell out to a Playwright-driven Python
//! script (see `src/lib.rs`'s module doc-comment), but this sandbox
//! has neither a captured Reddit session nor (usually) Playwright
//! itself installed, so a real run still can't succeed — it just fails
//! for a real reason now (no session cookies, no Playwright package,
//! or no live network) instead of a canned "not implemented yet"
//! message. No items ever land in storage for this connector in CI.
//!
//! So the second test below builds exactly the same "fully valid
//! input" the in-process unit test
//! `fetch_with_a_valid_but_empty_session_dir_fails_cleanly`
//! (`src/lib.rs`) does — a real temp directory standing in for a
//! captured session, with the `REDDIT_SESSION_DIR` secret set and
//! pointing at it — and proves that when it's driven through the real
//! subprocess boundary instead of an in-process `fetch()` call, a
//! connector-level error still comes back out the other side: the
//! run/stream bridge correctly relays it end to end. Which exact error
//! (missing Playwright vs. missing interpreter vs. a dead session) is
//! environment-dependent, so this only asserts the invariant that
//! holds regardless: some error, and no items committed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-reddit"))
}

fn candidate() -> ConnectorCandidate {
    ConnectorCandidate {
        dist_name: "rusty_dbs".to_string(),
        is_builtin: true,
        command: binary_path(),
        args: Vec::new(),
    }
}

/// A real temp directory standing in for a captured Playwright
/// session, the same way `src/lib.rs`'s `temp_dir` test helper builds
/// one for `fetch_with_a_valid_session_dir_is_blocked_pending_the_playwright_helper`.
fn temp_session_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-connector-reddit-subprocess-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_real_binarys_handshake_is_valid_and_matches_the_connector() {
    let mut registry = ConnectorRegistry::new();
    let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.loaded.len(), 1);
    let rc = &report.loaded[0];
    assert_eq!(rc.type_, "reddit");
    assert_eq!(
        rc.handshake.secret_keys,
        vec!["REDDIT_SESSION_DIR".to_string()]
    );
    assert!(!rc.handshake.item_kinds.is_empty());
    assert!(rc.handshake.item_kinds.contains(&"post".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"comment".to_string()));
    // This connector requires a captured, cookie-authenticated browser
    // session (see src/lib.rs's module doc-comment) — the handshake
    // must say so, matching `Connector::needs_playwright_browser`'s
    // `true` override in src/lib.rs.
    assert!(rc.handshake.needs_playwright_browser);
}

#[test]
fn a_real_run_with_a_fully_valid_session_dir_still_relays_a_clean_error() {
    let dir = temp_session_dir();

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("reddit").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-reddit", "reddit", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-reddit".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([(
            "REDDIT_SESSION_DIR".to_string(),
            dir.to_string_lossy().to_string(),
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

    // Proves the subprocess boundary correctly relays a
    // connector-level error end to end. This sandbox has no captured
    // Reddit session (and likely no Playwright package either), so
    // acquisition can't succeed — which exact error surfaces depends
    // on what's installed here, so only the non-empty-error and
    // no-items-committed invariants are asserted.
    let error = outcome.error.expect("expected an error, got none");
    assert!(!error.is_empty(), "{error}");
    assert_eq!(outcome.items_seen, 0);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.is_empty());
}
