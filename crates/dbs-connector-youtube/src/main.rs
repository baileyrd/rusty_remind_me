//! The `dbs-connector-youtube` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for one real connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing youtube-specific here beyond
//! constructing the connector itself.
//!
//! `DBS_YOUTUBE_TEST_YT_DLP_BIN`, if set, overrides the `yt-dlp`
//! binary/path this connector shells out to — test-only, playing the
//! same role raindrop's `DBS_RAINDROP_TEST_BASE_URL` plays for that
//! connector's mock HTTP server. The redirect target differs because
//! this connector has no HTTP layer at all: every fetch is a
//! subprocess call to `yt-dlp`, not an HTTP request, so integration
//! tests point a real spawned binary at a fake `yt-dlp` shell script
//! on disk instead of a mock server URL.

use dbs_connector_youtube::{YouTubeConfig, YouTubeConnector};

fn main() {
    let mut connector = YouTubeConnector::new(YouTubeConfig::default());
    if let Ok(yt_dlp_bin) = std::env::var("DBS_YOUTUBE_TEST_YT_DLP_BIN") {
        connector = connector.with_yt_dlp_bin(yt_dlp_bin);
    }
    dbs_connector_support::run_connector_main(&mut connector);
}
