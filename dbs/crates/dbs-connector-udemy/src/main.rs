//! The `dbs-connector-udemy` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for the Udemy connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing udemy-specific here beyond
//! constructing the connector itself.
//!
//! `DBS_UDEMY_TEST_BASE_URL`, if set, overrides the Udemy REST API
//! base URL (default `https://www.udemy.com`) — test-only, so
//! integration tests can point a real spawned binary at a local mock
//! HTTP server instead of the live API.
//!
//! The `download_videos`/`yt-dlp` path (`UdemyConnector::with_yt_dlp_bin`)
//! is deliberately not wired up to an env var here: `UdemyConfig::default()`
//! has `download_videos: false`, so that path is unreachable with a
//! default-config real run and is already covered by this crate's own
//! unit tests (`src/lib.rs`), which exercise it directly against the
//! `Connector` trait with a fake `yt-dlp` script on disk.

use dbs_connector_udemy::{UdemyConfig, UdemyConnector};

fn main() {
    let mut connector = UdemyConnector::new(UdemyConfig::default());
    if let Ok(base_url) = std::env::var("DBS_UDEMY_TEST_BASE_URL") {
        connector = connector.with_base_url(base_url);
    }
    dbs_connector_support::run_connector_main(&mut connector);
}
