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
//! without this override the binary can't do a real run in *this
//! integration test file* specifically (it never sets a real source
//! config). A real `dbs backup` run supplies `instance` from
//! `sources.<name>.instance` via `MastodonConnector::configure()`,
//! called by `dbs_connector_support::run_connector_main` with the
//! per-source wire config on every real run (#166/ADR-0002) — this
//! env var exists only for tests that spawn the binary directly.
use dbs_connector_mastodon::{MastodonConfig, MastodonConnector};

fn main() {
    let mut config = MastodonConfig::default();
    if let Ok(base_url) = std::env::var("DBS_MASTODON_TEST_BASE_URL") {
        config.instance = base_url;
    }
    let mut connector = MastodonConnector::new(config);
    dbs_connector_support::run_connector_main(&mut connector);
}
