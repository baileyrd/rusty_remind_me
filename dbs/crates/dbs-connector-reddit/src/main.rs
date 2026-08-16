//! The `dbs-connector-reddit` subprocess binary (issue #164, part of
//! the same remaining-connector-binaries sweep as #161's raindrop
//! binary) — proves ADR-0001's protocol end to end for this connector,
//! via `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing reddit-specific here beyond
//! constructing the connector itself.
//!
//! **Why there's no test-only base-URL override here, unlike
//! `dbs-connector-raindrop`'s `main.rs`:** that connector's `fetch()`
//! makes real HTTP calls, so its binary needs a way for integration
//! tests to redirect those calls at a local mock server instead of the
//! live API. This connector's acquisition step (issue #187) instead
//! shells out to a Python/Playwright subprocess that drives a real
//! Chromium page against reddit.com — there is no HTTP layer here at
//! all to redirect at a mock server, and no realistic way to fake a
//! whole browser session in a subprocess-boundary integration test.
//! `tests/subprocess_binary_integration.rs` proves the parts that
//! *are* provable without one: the handshake, and that a connector-
//! level error (there being no real captured session in CI) relays
//! correctly through the real run/stream subprocess boundary.

use dbs_connector_reddit::{RedditConfig, RedditConnector};

fn main() {
    let mut connector = RedditConnector::new(RedditConfig::default());
    dbs_connector_support::run_connector_main(&mut connector);
}
