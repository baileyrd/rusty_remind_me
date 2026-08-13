//! Real-subprocess integration tests for the `dbs-connector-pocketcasts`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Pocket Casts
//! web-player API served.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-pocketcasts"))
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
    assert_eq!(rc.type_, "pocketcasts");
    assert_eq!(
        rc.handshake.secret_keys,
        vec![
            "POCKETCASTS_EMAIL".to_string(),
            "POCKETCASTS_PASSWORD".to_string()
        ]
    );
    assert!(rc.handshake.item_kinds.contains(&"podcast".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"starred".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"history".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

fn login_body() -> String {
    serde_json::json!({"token": "bearer-tok"}).to_string()
}

fn podcast_json(uuid: &str, title: &str) -> serde_json::Value {
    serde_json::json!({"uuid": uuid, "title": title, "description": "a podcast"})
}

fn episode_json(uuid: &str, title: &str, published: &str) -> serde_json::Value {
    serde_json::json!({
        "uuid": uuid,
        "title": title,
        "published": published,
        "shareUrl": format!("https://pca.st/episode/{uuid}"),
    })
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();

    // Mirrors src/lib.rs's own `full_fetch_yields_all_kinds_and_a_reconcile_marker`
    // unit test: a login exchange plus the three list endpoints
    // (subscriptions/starred/history), each returning one record.
    let _m_login = server
        .mock("POST", "/user/login")
        .with_status(200)
        .with_body(login_body())
        .create();
    let podcasts = serde_json::json!({"podcasts": [podcast_json("p1", "A Podcast")]});
    let starred = serde_json::json!({
        "episodes": [episode_json("s1", "Starred Ep", "2024-06-01T00:00:00Z")]
    });
    let history = serde_json::json!({
        "episodes": [episode_json("h1", "History Ep", "2024-06-02T00:00:00Z")]
    });
    let _m_podcasts = server
        .mock("POST", "/user/podcast/list")
        .with_status(200)
        .with_body(podcasts.to_string())
        .create();
    let _m_starred = server
        .mock("POST", "/user/starred")
        .with_status(200)
        .with_body(starred.to_string())
        .create();
    let _m_history = server
        .mock("POST", "/user/history")
        .with_status(200)
        .with_body(history.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_POCKETCASTS_TEST_BASE_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock server instead of the live Pocket Casts API.
    std::env::set_var("DBS_POCKETCASTS_TEST_BASE_URL", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("pocketcasts").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-pocketcasts", "pocketcasts", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-pocketcasts".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([
            (
                "POCKETCASTS_EMAIL".to_string(),
                "me@example.com".to_string(),
            ),
            ("POCKETCASTS_PASSWORD".to_string(), "password".to_string()),
        ]),
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
    std::env::remove_var("DBS_POCKETCASTS_TEST_BASE_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 3);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("podcast:p1"));
    assert!(live.contains("starred:s1"));
    assert!(live.contains("history:h1"));
}
