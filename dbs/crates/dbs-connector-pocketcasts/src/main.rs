//! The `dbs-connector-pocketcasts` subprocess binary (issue #164) —
//! proves ADR-0001's protocol end to end for one real connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing pocketcasts-specific here
//! beyond constructing the connector itself.
//!
//! `DBS_POCKETCASTS_TEST_BASE_URL`, if set, overrides the Pocket
//! Casts API base URL — test-only, so integration tests can point a
//! real spawned binary at a local mock HTTP server instead of the
//! live (and unofficial, reverse-engineered) API.

use dbs_connector_pocketcasts::{PocketCastsConfig, PocketCastsConnector};

fn main() {
    let mut connector = PocketCastsConnector::new(PocketCastsConfig::default());
    if let Ok(base_url) = std::env::var("DBS_POCKETCASTS_TEST_BASE_URL") {
        connector = connector.with_api_base(base_url);
    }
    dbs_connector_support::run_connector_main(&mut connector);
}
