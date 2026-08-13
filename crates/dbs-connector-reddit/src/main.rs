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
//! live API. This connector has no such thing to redirect. As
//! `src/lib.rs`'s module doc-comment explains, saved-post/comment
//! acquisition requires a cookie-authenticated Playwright browser
//! session, and that capability is blocked pending issue #99 (the
//! shared Playwright launch helper) — `fetch()` validates its
//! `session_dir_env` config, checks the secret is set, and confirms
//! the session directory exists on disk, then **unconditionally**
//! returns a `ConnectorError::Config` pointing at #99, regardless of
//! how valid that input is. There is no HTTP layer, no mock server,
//! and no `with_base_url`-style hook to add, because there is no live
//! call to intercept.
//!
//! So why does this binary exist at all, if the connector can't do
//! real work yet? Because "wired up as a real subprocess binary" and
//! "can fetch real data" are two different milestones, and this issue
//! is only the first one: this binary makes the connector genuinely
//! discoverable (`ConnectorRegistry::discover` can spawn it, handshake
//! with it, and see its true `needs_playwright_browser` capability)
//! and makes its current, always-an-error behavior provable end to
//! end through the real subprocess boundary — not just from the
//! in-process unit tests `src/lib.rs` already has. When #99 lands and
//! `fetch()` is rewritten to actually acquire data, this `main.rs`
//! won't need to change at all.

use dbs_connector_reddit::{RedditConfig, RedditConnector};

fn main() {
    let mut connector = RedditConnector::new(RedditConfig::default());
    dbs_connector_support::run_connector_main(&mut connector);
}
