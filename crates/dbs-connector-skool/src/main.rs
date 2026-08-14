//! The `dbs-connector-skool` subprocess binary (part of issue #164) —
//! makes this connector genuinely discoverable via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses; see that crate's `subprocess_main` module for what
//! this actually does), exactly like `dbs-connector-raindrop` (#161).
//!
//! Unlike raindrop (and most of its siblings), there's no
//! `with_base_url`-style override here, and none is needed: this
//! connector makes zero outbound HTTP calls anywhere — no `reqwest`
//! dependency exists in this crate at all. `SkoolConnector::fetch()`'s
//! acquisition step (issue #188) instead shells out to a
//! Python/Playwright subprocess that drives real Chromium pages
//! against skool.com — there is no HTTP layer here at all to redirect
//! at a mock server, and no realistic way to fake a whole browser
//! session in a subprocess-boundary integration test.
//! `tests/subprocess_binary_integration.rs` proves the parts that
//! *are* provable without one: the handshake, and that a connector-
//! level error (there being no real captured session in CI) relays
//! correctly through the real run/stream subprocess boundary.

use dbs_connector_skool::{SkoolConfig, SkoolConnector};

fn main() {
    let mut connector = SkoolConnector::new(SkoolConfig::default());
    dbs_connector_support::run_connector_main(&mut connector);
}
