//! Real-subprocess integration tests for the `dbs-connector-podcast`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the episode a mock RSS feed served.
//!
//! Podcast has no fixed API host to redirect (see `src/main.rs`'s
//! doc comment): `DBS_PODCAST_TEST_FEED_URL` stands in for the whole
//! `feeds` list, pointing the spawned binary's single configured feed
//! at a local `mockito` server instead of a real podcast feed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-podcast"))
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
    assert_eq!(rc.type_, "podcast");
    // No account or token needed — feeds are public.
    assert!(rc.handshake.secret_keys.is_empty());
    assert!(rc.handshake.item_kinds.contains(&"episode".to_string()));
    // Rolling-window feeds never get swept (see `src/lib.rs`'s module
    // docstring): full enumeration/deletion detection is deliberately
    // unsupported.
    assert!(!rc.handshake.capabilities.supports_full_enumeration);
}

// Reused verbatim from `src/lib.rs`'s own
// `a_single_rss_feed_is_parsed_into_episodes` unit test, so this
// real-subprocess run is proven against the exact same fixture shape
// the connector's own unit tests already exercise.
const RSS_FEED: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>A Show</title>
    <item>
      <guid>ep-1</guid>
      <title>Episode One</title>
      <link>https://example.com/ep1</link>
      <description>Show notes for episode one</description>
      <pubDate>Wed, 01 Jun 2024 12:00:00 +0000</pubDate>
      <enclosure url="https://example.com/ep1.mp3" type="audio/mpeg" length="12345"/>
      <itunes:duration>30:00</itunes:duration>
      <itunes:episode>1</itunes:episode>
    </item>
  </channel>
</rss>"#;

#[test]
fn a_real_run_against_a_mock_feed_commits_items_through_the_full_subprocess_boundary() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/feed.xml")
        .with_status(200)
        .with_body(RSS_FEED)
        .create();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no other
    // test in this binary reads or writes DBS_PODCAST_TEST_FEED_URL.
    // `std::process::Command` (used by `run_connector_subprocess`)
    // inherits it, which is how the real spawned binary is pointed at
    // the mock feed instead of a real podcast feed.
    std::env::set_var(
        "DBS_PODCAST_TEST_FEED_URL",
        format!("{}/feed.xml", server.url()),
    );

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("podcast").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-podcasts", "podcast", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-podcasts".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        // No `secret_keys()`, so nothing to pass — the podcast
        // connector has no auth (`requires_auth: false`).
        secrets: HashMap::new(),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, None).unwrap();
    std::env::remove_var("DBS_PODCAST_TEST_FEED_URL");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 1);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert_eq!(live.len(), 1);
    // The external id is `{feed-namespace}:{guid}` — the namespace is a
    // stable hash of the feed URL (see `feed_ns` in `src/lib.rs`), so
    // only the guid suffix is predictable here.
    assert!(live.iter().any(|id| id.ends_with(":ep-1")), "{live:?}");
}
