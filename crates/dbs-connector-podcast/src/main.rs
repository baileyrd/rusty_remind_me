//! The `dbs-connector-podcast` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for one real connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing podcast-specific here beyond
//! constructing the connector itself.
//!
//! Unlike most connectors (e.g. `dbs-connector-raindrop`'s
//! `DBS_RAINDROP_TEST_BASE_URL`), this connector has no fixed API host
//! to override — there's no `with_base_url`-style builder because
//! `PodcastConfig::feeds` is already a list of complete feed URLs,
//! fetched directly via `http.get(feed_url)`. The feed URLs themselves
//! *are* the redirect target. So `DBS_PODCAST_TEST_FEED_URL`, if set,
//! plays the same role: it overrides `config.feeds` with a single-entry
//! vec pointing at that URL — test-only, so integration tests can point
//! a real spawned binary at a local mock RSS/Atom feed instead of a
//! real one.
//!
//! Note this is *not* a stand-in for real per-source config passthrough
//! (a separate, still-open gap outside this issue's scope): without
//! that plumbing, a real `dbs backup` run of this binary today
//! constructs `PodcastConfig::default()` — `feeds: vec![]` — so it has
//! no feeds configured at all and will find nothing. This env var only
//! exists to make the binary itself testable in isolation.

use dbs_connector_podcast::{PodcastConfig, PodcastConnector};

fn main() {
    let mut config = PodcastConfig::default();
    if let Ok(feed_url) = std::env::var("DBS_PODCAST_TEST_FEED_URL") {
        config.feeds = vec![feed_url];
    }
    let mut connector = PodcastConnector::new(config);
    dbs_connector_support::run_connector_main(&mut connector);
}
