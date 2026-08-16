//! The `dbs-connector-vimeo` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for one real connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing vimeo-specific here beyond
//! constructing the connector itself.
//!
//! `DBS_VIMEO_TEST_BASE_URL`, if set, overrides the Vimeo API base
//! URL (default `https://api.vimeo.com`) — test-only, so integration
//! tests can point a real spawned binary at a local mock HTTP server
//! instead of the live API.
//!
//! The `download_videos` / `yt-dlp` path (see `VimeoConnector`'s doc
//! comment) is off by default (`VimeoConfig::default().download_videos
//! == false`) and so isn't reachable from a run with default config;
//! it isn't wired up here for the same reason — there's no
//! `DBS_VIMEO_TEST_YT_DLP_BIN` override in this binary. That path is
//! already covered by `dbs-connector-vimeo`'s own unit tests
//! (`with_yt_dlp_bin`).

use dbs_connector_vimeo::{VimeoConfig, VimeoConnector};

fn main() {
    let mut connector = VimeoConnector::new(VimeoConfig::default());
    if let Ok(base_url) = std::env::var("DBS_VIMEO_TEST_BASE_URL") {
        connector = connector.with_base_url(base_url);
    }
    dbs_connector_support::run_connector_main(&mut connector);
}
