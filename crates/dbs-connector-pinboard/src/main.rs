//! The `dbs-connector-pinboard` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for one real connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing pinboard-specific here beyond
//! constructing the connector itself.
//!
//! `DBS_PINBOARD_TEST_BASE_URL`, if set, overrides the Pinboard API
//! base URL — test-only, so integration tests can point a real spawned
//! binary at a local mock HTTP server instead of the live API.

use dbs_connector_pinboard::{PinboardConfig, PinboardConnector};

fn main() {
    let mut connector = PinboardConnector::new(PinboardConfig::default());
    if let Ok(base_url) = std::env::var("DBS_PINBOARD_TEST_BASE_URL") {
        connector = connector.with_base_url(base_url);
    }
    dbs_connector_support::run_connector_main(&mut connector);
}
