//! The `dbs-connector-bluesky` subprocess binary (issue #164, closing
//! out the same "not wired up" boundary this crate's `lib.rs` doc
//! comment calls out) — proves ADR-0001's protocol end to end for this
//! connector, via `dbs_connector_support::run_connector_main` (the
//! shared handshake+run/stream implementation every
//! `dbs-connector-<type>` binary uses), the same way issue #161 wired
//! up `dbs-connector-raindrop`. See that crate's `subprocess_main`
//! module for what this actually does; there's nothing
//! bluesky-specific here beyond constructing the connector itself.
//!
//! `DBS_BLUESKY_TEST_BASE_URL`, if set, overrides `BlueskyConfig::service`
//! (the AT Protocol PDS/service base URL, normally
//! `https://bsky.social`) — test-only, so integration tests can point a
//! real spawned binary at a local mock HTTP server instead of the live
//! Bluesky API.
//!
//! `BlueskyConfig::identifier` (the handle/DID whose likes to back
//! up) comes from a real source's `sources.<name>.identifier` config
//! via `BlueskyConnector::configure()`, called by
//! `dbs_connector_support::run_connector_main` with the per-source
//! wire config on every real run (#166/ADR-0002) — no env var or
//! override needed for that part.

use dbs_connector_bluesky::{BlueskyConfig, BlueskyConnector};

fn main() {
    let mut config = BlueskyConfig::default();
    if let Ok(base_url) = std::env::var("DBS_BLUESKY_TEST_BASE_URL") {
        config.service = base_url;
    }
    let mut connector = BlueskyConnector::new(config);
    dbs_connector_support::run_connector_main(&mut connector);
}
