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
//! dependency exists in this crate at all. `SkoolConnector::fetch()`
//! (see `src/lib.rs`) is permanently blocked pending issue #99 (the
//! shared Playwright launch helper): after validating everything that
//! doesn't need a live browser — the `video_cookies_file_env`
//! declaration, the `SKOOL_SESSION_DIR` secret, that the session
//! directory exists, that a downloads root resolves — it
//! unconditionally returns `Err(ConnectorError::Config(..))` naming
//! issue #99, regardless of what input it's given. There is thus no
//! mock HTTP target to redirect a test double at, and no config knob
//! this binary needs to plumb through.
//!
//! This binary exists anyway (rather than leaving the connector
//! un-wired) so that: (1) `dbs-cli`'s discovery can find `skool` as a
//! real subprocess like every other connector, proving its handshake
//! (secret keys, item kinds, `needs_playwright_browser`, ...) end to
//! end through ADR-0001's protocol; and (2) its current
//! always-an-error behavior is itself provable through the real
//! subprocess boundary — `tests/subprocess_binary_integration.rs`
//! spawns this exact binary and asserts the relayed error mentions
//! issue #99, so a future PR that lands #99 and wires up real
//! acquisition here will have a test that visibly starts failing
//! (forcing an update) rather than one that silently stays green.

use dbs_connector_skool::{SkoolConfig, SkoolConnector};

fn main() {
    let mut connector = SkoolConnector::new(SkoolConfig::default());
    dbs_connector_support::run_connector_main(&mut connector);
}
