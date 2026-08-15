//! Real-subprocess integration tests for the `dbs-connector-skool`
//! binary (part of issue #164) — proves ADR-0001's protocol end to
//! end for this connector: spawns the actual compiled binary, drives
//! it through `dbs_core::registry`'s handshake discovery (#45) and
//! `dbs_core::run_stream`'s run/stream bridge (#157) exactly the way
//! `dbs-cli` would.
//!
//! Unlike a fully-implemented connector (e.g. `dbs-connector-raindrop`,
//! #161), there's no mock HTTP server here and no `mockito` dev
//! dependency — `SkoolConnector` makes zero outbound HTTP calls;
//! since #188, `fetch()` really does shell out to a Playwright-driven
//! Python script (see `src/lib.rs`'s module doc-comment), but this
//! sandbox has neither a captured Skool session nor (usually)
//! Playwright itself installed, so a real run still can't succeed —
//! it just fails for a real reason now instead of a canned "not
//! implemented yet" message. So the second test below builds a
//! genuinely "fully valid" run — a real session directory that
//! exists, a real downloads directory, and the one secret `fetch()`
//! actually reads (`SKOOL_SESSION_DIR`) — the same way `src/lib.rs`'s
//! `fetch_with_everything_valid_but_no_real_session_fails_cleanly`
//! unit test does, and asserts the run/stream bridge relays a
//! connector-level error back through the real subprocess boundary.
//! Which exact error is environment-dependent, so only the
//! non-empty-error and no-items-committed invariants are asserted.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-skool"))
}

fn candidate() -> ConnectorCandidate {
    ConnectorCandidate {
        dist_name: "rusty_dbs".to_string(),
        is_builtin: true,
        command: binary_path(),
        args: Vec::new(),
    }
}

/// A fresh, real, existing temp directory — used for both the
/// captured-session dir and the downloads dir below, mirroring
/// `src/lib.rs`'s own `tests::temp_dir` helper (that one isn't
/// public, so this is a same-shaped duplicate rather than a shared
/// import across the crate boundary).
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-connector-skool-subprocess-test-{label}-{}-{:?}",
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
    assert_eq!(rc.type_, "skool");
    assert_eq!(
        rc.handshake.secret_keys,
        vec![
            "SKOOL_SESSION_DIR".to_string(),
            "YOUTUBE_COOKIES_FILE".to_string(),
            "GITHUB_TOKEN".to_string(),
        ]
    );
    assert!(!rc.handshake.item_kinds.is_empty());
    assert_eq!(
        rc.handshake.item_kinds,
        vec![
            "community".to_string(),
            "course".to_string(),
            "lesson".to_string(),
        ]
    );
    assert!(rc.handshake.needs_playwright_browser);
}

#[test]
fn a_real_run_with_fully_valid_input_relays_a_clean_error_through_the_subprocess_boundary() {
    let session = temp_dir("valid-session");
    let downloads = temp_dir("valid-downloads");

    let mut registry = ConnectorRegistry::new();
    let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let rc = registry.get("skool").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-skool", "skool", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-skool".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        // Only SKOOL_SESSION_DIR is actually read by fetch(): the
        // video_cookies_file_env check only verifies the *name* is
        // one of the declared secret_keys, it never reads
        // YOUTUBE_COOKIES_FILE's value, and GITHUB_TOKEN isn't
        // touched at all pending #99's real acquisition step.
        secrets: HashMap::from([(
            "SKOOL_SESSION_DIR".to_string(),
            session.to_string_lossy().to_string(),
        )]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: Some(downloads),
        config: HashMap::new(),
        http_timeout: 30.0,
        http_rate_limit_per_min: 0,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, None).unwrap();

    let error = outcome.error.expect("expected a relayed connector error");
    assert!(!error.is_empty(), "{error}");
    assert_eq!(outcome.items_seen, 0);
}
