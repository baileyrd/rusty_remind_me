//! The `dbs-connector-mastodon` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for one more real connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing mastodon-specific here beyond
//! constructing the connector itself.
//!
//! `DBS_MASTODON_TEST_BASE_URL`, if set, overrides `MastodonConfig`'s
//! `instance` — test-only, so integration tests can point a real
//! spawned binary at a local mock HTTP server instead of a live
//! Mastodon instance.
//!
//! Unlike `dbs-connector-raindrop` (#161), where the default config
//! already points at the real Raindrop API and the env override only
//! matters for tests, `MastodonConfig::default()`'s `instance` is
//! `""` — there's no single default Mastodon instance to fall back
//! to, since each account picks its own. `fetch()` rejects an empty
//! `instance` outright (it must start with `http://`/`https://`), so
//! without this override the binary can't do *any* real run today,
//! test or otherwise. A real per-source config passthrough — still a
//! gap here, same as raindrop's — would supply a real instance URL
//! instead of relying on this env var.
use dbs_connector_mastodon::{MastodonConfig, MastodonConnector};

fn main() {
    let mut config = MastodonConfig::default();
    if let Ok(base_url) = std::env::var("DBS_MASTODON_TEST_BASE_URL") {
        config.instance = base_url;
    }
    let mut connector = MastodonConnector::new(config);
    dbs_connector_support::run_connector_main(&mut connector);
}
