//! Real-subprocess integration tests for the `dbs-connector-readwise`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Readwise API served.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-readwise"))
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
    assert_eq!(rc.type_, "readwise");
    assert_eq!(rc.handshake.secret_keys, vec!["READWISE_TOKEN".to_string()]);
    assert!(rc.handshake.item_kinds.contains(&"book".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"highlight".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

// Mirrors `book_json`/`highlight_json`/`page` in
// dbs-connector-readwise's own `src/lib.rs` unit tests — same fixture
// shapes, since this is exercising the same `fetch()` logic, just
// through the compiled binary + wire protocol instead of calling it
// in-process.

fn book_json(id: i64, title: &str, updated: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "title": title,
        "source_url": "https://example.com/book",
        "author": "An Author",
        "tags": [{"name": "nonfiction"}],
        "last_highlight_at": updated,
        "updated": updated,
    })
}

fn highlight_json(id: i64, text: &str, updated: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "text": text,
        "url": "https://example.com/highlight",
        "tags": [{"name": "quote"}],
        "highlighted_at": updated,
        "updated": updated,
    })
}

fn page(results: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "count": results.len(),
        "next": null,
        "results": results,
    })
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();
    let books = page(vec![book_json(1, "A Book", "2024-06-01T00:00:00Z")]);
    let highlights = page(vec![highlight_json(2, "a quote", "2024-06-02T00:00:00Z")]);
    let _m_books = server
        .mock("GET", mockito::Matcher::Regex(r"^/books/\?.*".to_string()))
        .with_status(200)
        .with_body(books.to_string())
        .create();
    let _m_highlights = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/highlights/\?.*".to_string()),
        )
        .with_status(200)
        .with_body(highlights.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_READWISE_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock server instead of the live Readwise API.
    std::env::set_var("DBS_READWISE_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("readwise").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-readwise", "readwise", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-readwise".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([("READWISE_TOKEN".to_string(), "secret-token".to_string())]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
        config: HashMap::new(),
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, None).unwrap();
    std::env::remove_var("DBS_READWISE_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("book:1"));
    assert!(live.contains("highlight:2"));
}
