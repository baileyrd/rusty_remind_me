//! Real-subprocess integration tests for the `dbs-connector-github`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock GitHub API served.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-github"))
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
    assert_eq!(rc.type_, "github");
    assert_eq!(rc.handshake.secret_keys, vec!["GITHUB_TOKEN".to_string()]);
    assert!(rc.handshake.item_kinds.contains(&"star".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"gist".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
    assert!(rc.handshake.capabilities.requires_auth);
}

fn star_json(repo_id: i64, full_name: &str, starred_at: &str) -> serde_json::Value {
    serde_json::json!({
        "starred_at": starred_at,
        "repo": {
            "id": repo_id,
            "full_name": full_name,
            "html_url": format!("https://github.com/{full_name}"),
            "description": "a repo",
            "topics": ["rust"],
            "language": "Rust",
        }
    })
}

fn gist_json(id: &str, description: &str, updated_at: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "description": description,
        "html_url": format!("https://gist.github.com/{id}"),
        "created_at": updated_at,
        "updated_at": updated_at,
        "files": {"a.rs": {"language": "Rust"}},
    })
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();
    // Mirrors `full_fetch_yields_stars_and_gists_and_a_combined_reconcile_marker`
    // in `dbs-connector-github`'s own unit tests: one star, one gist,
    // then an empty second page for each so pagination terminates.
    let stars_page0 = serde_json::json!([star_json(1, "me/repo", "2024-06-01T00:00:00Z")]);
    let empty = serde_json::json!([]);
    let gists_page0 = serde_json::json!([gist_json("g1", "my gist", "2024-06-02T00:00:00Z")]);

    let _m_stars0 = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/user/starred\?.*page=1.*".to_string()),
        )
        .with_status(200)
        .with_body(stars_page0.to_string())
        .create();
    let _m_stars1 = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/user/starred\?.*page=2.*".to_string()),
        )
        .with_status(200)
        .with_body(empty.to_string())
        .create();
    let _m_gists0 = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/gists\?.*page=1.*".to_string()),
        )
        .with_status(200)
        .with_body(gists_page0.to_string())
        .create();
    let _m_gists1 = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/gists\?.*page=2.*".to_string()),
        )
        .with_status(200)
        .with_body(empty.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_GITHUB_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock server instead of the live GitHub API.
    std::env::set_var("DBS_GITHUB_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("github").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-github", "github", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-github".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([("GITHUB_TOKEN".to_string(), "secret-token".to_string())]),
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

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, None).unwrap();
    std::env::remove_var("DBS_GITHUB_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("star:1"));
    assert!(live.contains("gist:g1"));
}
