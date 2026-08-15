//! Real-subprocess integration tests for the `dbs-connector-spotify`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a mock Spotify API served.
//!
//! Spotify is the one genuinely OAuth-shaped connector in the set: a
//! run starts with a refresh-token exchange against the token
//! endpoint, then hits the Web API with the resulting access token
//! (see `dbs-connector-spotify`'s crate doc). That means a real run
//! needs BOTH outbound hosts mocked, not just one — this test points
//! `DBS_SPOTIFY_TEST_TOKEN_URL` and `DBS_SPOTIFY_TEST_API_BASE` at the
//! same `mockito::Server` instance, which happily serves distinct
//! paths (`/api/token` vs `/me/tracks`, `/me/playlists`) off one
//! listener — exactly like the crate's own `connector_for` test
//! helper in `src/lib.rs` does in-process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-spotify"))
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
    assert_eq!(rc.type_, "spotify");
    assert_eq!(
        rc.handshake.secret_keys,
        vec![
            "SPOTIFY_CLIENT_ID".to_string(),
            "SPOTIFY_CLIENT_SECRET".to_string(),
            "SPOTIFY_REFRESH_TOKEN".to_string(),
        ]
    );
    assert!(rc.handshake.item_kinds.contains(&"track".to_string()));
    assert!(rc.handshake.item_kinds.contains(&"playlist".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

fn token_body() -> String {
    serde_json::json!({"access_token": "access-tok"}).to_string()
}

fn track_entry(id: &str, name: &str, artist: &str, added_at: &str) -> serde_json::Value {
    serde_json::json!({
        "added_at": added_at,
        "track": {
            "id": id,
            "name": name,
            "artists": [{"name": artist}],
            "external_urls": {"spotify": format!("https://open.spotify.com/track/{id}")},
            "album": {"name": "An Album"},
        }
    })
}

fn playlist_entry(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "description": "a playlist",
        "external_urls": {"spotify": format!("https://open.spotify.com/playlist/{id}")},
    })
}

fn page(items: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({"items": items, "next": null})
}

#[test]
fn a_real_run_against_a_mock_api_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();

    // Token exchange: the connector's very first outbound call, hit
    // once at the start of the run regardless of which item kinds are
    // enabled.
    let _m_token = server
        .mock("POST", "/api/token")
        .with_status(200)
        .with_body(token_body())
        .create();

    let tracks = page(vec![track_entry(
        "t1",
        "A Song",
        "An Artist",
        "2024-06-01T00:00:00Z",
    )]);
    let playlists = page(vec![playlist_entry("p1", "A Playlist")]);
    let _m_tracks = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/me/tracks\?.*".to_string()),
        )
        .with_status(200)
        .with_body(tracks.to_string())
        .create();
    let _m_playlists = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/me/playlists\?.*".to_string()),
        )
        .with_status(200)
        .with_body(playlists.to_string())
        .create();

    // SAFETY (test-only, single-threaded w.r.t. these env vars): no
    // other test in this binary reads or writes
    // DBS_SPOTIFY_TEST_TOKEN_URL / DBS_SPOTIFY_TEST_API_BASE.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits them, which is how the real spawned binary is pointed
    // at the mock server instead of the live Spotify hosts. One
    // `mockito::Server` instance serves both — token exchange and Web
    // API calls land on distinct paths of the same listener.
    std::env::set_var(
        "DBS_SPOTIFY_TEST_TOKEN_URL",
        format!("{}/api/token", server.url()),
    );
    std::env::set_var("DBS_SPOTIFY_TEST_API_BASE", server.url());

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("spotify").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-spotify", "spotify", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-spotify".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([
            ("SPOTIFY_CLIENT_ID".to_string(), "client-id".to_string()),
            (
                "SPOTIFY_CLIENT_SECRET".to_string(),
                "client-secret".to_string(),
            ),
            (
                "SPOTIFY_REFRESH_TOKEN".to_string(),
                "refresh-token".to_string(),
            ),
        ]),
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
    std::env::remove_var("DBS_SPOTIFY_TEST_TOKEN_URL");
    std::env::remove_var("DBS_SPOTIFY_TEST_API_BASE");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("track:t1"));
    assert!(live.contains("playlist:p1"));
}
