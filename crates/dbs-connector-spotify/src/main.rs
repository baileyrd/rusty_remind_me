//! The `dbs-connector-spotify` subprocess binary (issue #164) — proves
//! ADR-0001's protocol end to end for the Spotify connector, via
//! `dbs_connector_support::run_connector_main` (the shared
//! handshake+run/stream implementation every `dbs-connector-<type>`
//! binary uses). See that crate's `subprocess_main` module for what
//! this actually does; there's nothing spotify-specific here beyond
//! constructing the connector itself.
//!
//! Unlike most connectors (one outbound host, one override env var),
//! Spotify talks to two: `accounts.spotify.com` for the OAuth
//! refresh-token exchange and `api.spotify.com` for the Web API
//! itself (see `src/lib.rs`'s module doc for why — access tokens are
//! short-lived, so every run starts with a refresh). Both need to be
//! redirectable independently for tests to point a real spawned
//! binary at a local mock server instead of the live services:
//!
//! - `DBS_SPOTIFY_TEST_API_BASE`, if set, overrides the Web API base
//!   URL (default `https://api.spotify.com/v1`).
//! - `DBS_SPOTIFY_TEST_TOKEN_URL`, if set, overrides the token
//!   endpoint URL (default `https://accounts.spotify.com/api/token`).
//!
//! Both are test-only; a real deployment never sets either and the
//! connector talks to the live Spotify hosts.

use dbs_connector_spotify::{SpotifyConfig, SpotifyConnector};

fn main() {
    let mut connector = SpotifyConnector::new(SpotifyConfig::default());
    if let Ok(api_base) = std::env::var("DBS_SPOTIFY_TEST_API_BASE") {
        connector = connector.with_api_base(api_base);
    }
    if let Ok(token_url) = std::env::var("DBS_SPOTIFY_TEST_TOKEN_URL") {
        connector = connector.with_token_url(token_url);
    }
    dbs_connector_support::run_connector_main(&mut connector);
}
