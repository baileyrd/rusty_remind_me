//! Spotify connector: backs up your liked songs and playlists (issue
//! #91). Mirrors `dbs.connectors.spotify` in
//! baileyrd/Daily-Backup-System.
//!
//! Auth is the one genuinely OAuth-shaped flow in the connector set:
//! Spotify access tokens live ~1 hour, so the durable secret is a
//! **refresh token** (plus the app's client id/secret), exchanged for
//! a fresh access token at the start of every run. Getting the
//! refresh token is a one-time manual dance (create an app at
//! developer.spotify.com, authorize with the `user-library-read
//! playlist-read-private` scopes, capture the refresh token); after
//! that, runs are fully unattended.
//!
//! Strategy mirrors `github`'s stars: `/v1/me/tracks` returns liked
//! songs newest-first with an `added_at` per entry, so incremental
//! runs early-stop below the stored watermark (with overlap).
//! Playlists are a small catalog listed fully each run. `raw` stays
//! verbatim; the nested `track` object and playlist
//! `snapshot_id`/`images`/`tracks` are volatile (popularity scores,
//! rotating CDN image URLs, and count wrappers churn constantly)
//! while meaningful changes still hash via the semantic projection.
//!
//! Deletion detection: reconcile/full enumerates both kinds and
//! yields one [`ReconcileMarker`]; disabled kinds withhold it.
//!
//! Reachable from a real `dbs backup spotify` run since #164's
//! `dbs-connector-spotify` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

const DEFAULT_API_BASE: &str = "https://api.spotify.com/v1";
const DEFAULT_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// Re-fetch window below the stored watermark, matching `raindrop`
/// and `github`: clocks skew; the idempotent upsert dedups the
/// overlap.
const OVERLAP_SECONDS: i64 = 300;

#[derive(Debug, Clone)]
pub struct SpotifyConfig {
    pub include_liked_tracks: bool,
    pub include_playlists: bool,
    /// API page size, max 50.
    pub page_size: u32,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            include_liked_tracks: true,
            include_playlists: true,
            page_size: 50,
        }
    }
}

pub struct SpotifyConnector {
    config: SpotifyConfig,
    api_base: String,
    token_url: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl SpotifyConnector {
    pub fn new(config: SpotifyConfig) -> Self {
        Self {
            config,
            api_base: DEFAULT_API_BASE.to_string(),
            token_url: DEFAULT_TOKEN_URL.to_string(),
            secret_keys: vec![
                "SPOTIFY_CLIENT_ID".to_string(),
                "SPOTIFY_CLIENT_SECRET".to_string(),
                "SPOTIFY_REFRESH_TOKEN".to_string(),
            ],
            item_kinds: vec![
                ItemKind {
                    name: "track".to_string(),
                    display_name: "Liked song".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "playlist".to_string(),
                    display_name: "Playlist".to_string(),
                    description: String::new(),
                },
            ],
            // track.popularity / playlist snapshot ids / CDN image
            // URLs churn without the saved content changing; semantic
            // fields carry the meaningful bits.
            volatile_fields: vec![
                "track".to_string(),
                "snapshot_id".to_string(),
                "images".to_string(),
                "tracks".to_string(),
            ],
        }
    }

    /// Overrides the API base URL (default
    /// `https://api.spotify.com/v1`) — for tests to point at a local
    /// mock server.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// Overrides the token endpoint URL (default
    /// `https://accounts.spotify.com/api/token`) — for tests to point
    /// at a local mock server.
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    fn access_token(
        &self,
        http: &RefCell<ManagedHttpClient>,
        client_id: &str,
        client_secret: &str,
        refresh: &str,
    ) -> Result<String, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(reqwest::Method::POST, &self.token_url, |b| {
                b.basic_auth(client_id, Some(client_secret))
                    .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])
            })
            .map_err(classify_token_error)?;
        let payload: Value = response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))?;
        payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                ConnectorError::Auth("Spotify token refresh returned no access_token".to_string())
            })
    }

    fn get_json(
        &self,
        http: &RefCell<ManagedHttpClient>,
        url: &str,
        token: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, url, |b| {
                b.bearer_auth(token).query(params)
            })
            .map_err(classify_api_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    fn fetch_tracks(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        cursor: &mut serde_json::Map<String, Value>,
        full: bool,
        live_ids: &mut HashSet<String>,
        out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    ) {
        let high = if full {
            None
        } else {
            cursor
                .get("tracks_high_watermark")
                .and_then(|v| v.as_str())
                .and_then(|s| parse_iso(Some(s)))
        };
        let mut max_seen: Option<String> = cursor
            .get("tracks_high_watermark")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let mut offset = 0u32;
        let mut page = 1u32;
        let mut stop = false;

        while !stop {
            let url = format!("{}/me/tracks", self.api_base);
            let params = [
                ("limit", self.config.page_size.to_string()),
                ("offset", offset.to_string()),
            ];
            let payload = match self.get_json(http, &url, token, &params) {
                Ok(p) => p,
                Err(e) => {
                    out.push(Err(e));
                    return;
                }
            };
            let entries = payload
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for entry in &entries {
                let added_at = entry
                    .get("added_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let ts = parse_iso(Some(&added_at));
                if let (Some(h), Some(t)) = (high, ts) {
                    if t < h - chrono::Duration::seconds(OVERLAP_SECONDS) {
                        stop = true;
                        break;
                    }
                }
                let Some(item) = track_item(entry) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                if !added_at.is_empty() && max_seen.as_deref().is_none_or(|m| added_at.as_str() > m)
                {
                    max_seen = Some(added_at.clone());
                }
                out.push(Ok(FetchEvent::Item(item)));
            }
            // Mid-phase checkpoints carry the OLD watermark: advancing
            // it before the walk completes would let a crash skip the
            // gap between the old mark and the last committed page
            // forever.
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(cursor.clone()),
                },
                note: format!("tracks page {page}"),
            })));
            let has_next = payload.get("next").is_some_and(|v| !v.is_null());
            if stop || !has_next || entries.is_empty() {
                break;
            }
            offset += entries.len() as u32;
            page += 1;
        }
        if let Some(seen) = max_seen {
            cursor.insert("tracks_high_watermark".to_string(), Value::String(seen));
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(cursor.clone()),
                },
                note: "tracks done".to_string(),
            })));
        }
    }

    fn fetch_playlists(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        live_ids: &mut HashSet<String>,
        out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    ) {
        let mut offset = 0u32;
        let mut page = 1u32;
        loop {
            let url = format!("{}/me/playlists", self.api_base);
            let params = [
                ("limit", self.config.page_size.to_string()),
                ("offset", offset.to_string()),
            ];
            let payload = match self.get_json(http, &url, token, &params) {
                Ok(p) => p,
                Err(e) => {
                    out.push(Err(e));
                    return;
                }
            };
            let entries = payload
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for pl in &entries {
                let Some(item) = playlist_item(pl) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(serde_json::Map::new()),
                },
                note: format!("playlists page {page}"),
            })));
            let has_next = payload.get("next").is_some_and(|v| !v.is_null());
            if !has_next || entries.is_empty() {
                break;
            }
            offset += entries.len() as u32;
            page += 1;
        }
    }
}

fn track_item(entry: &Value) -> Option<BackupItem> {
    let track = entry.get("track").cloned().unwrap_or(Value::Null);
    let tid = track
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?; // local files have no catalog id
    let artists: String = track
        .get("artists")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|artist| artist.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let name = track.get("name").and_then(|v| v.as_str());
    let title = if artists.is_empty() {
        name.map(str::to_string)
    } else {
        Some(format!("{artists} — {}", name.unwrap_or_default()))
    };
    let mut item = BackupItem::new(format!("track:{tid}"), "track", entry.clone()).ok()?;
    item.title = title;
    item.url = track
        .get("external_urls")
        .and_then(|v| v.get("spotify"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.body = track
        .get("album")
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.created_at = entry
        .get("added_at")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    Some(item)
}

fn playlist_item(pl: &Value) -> Option<BackupItem> {
    let id = pl
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let mut item = BackupItem::new(format!("playlist:{id}"), "playlist", pl.clone()).ok()?;
    item.title = pl.get("name").and_then(|v| v.as_str()).map(str::to_string);
    item.url = pl
        .get("external_urls")
        .and_then(|v| v.get("spotify"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.body = pl
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(item)
}

/// The token endpoint rejects a bad client id/secret/refresh token
/// with 400 or 401; everything else non-retryable is a transient
/// upstream problem.
fn classify_token_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(400 | 401) => ConnectorError::Auth(
                "Spotify refused the token refresh — check SPOTIFY_CLIENT_ID/SECRET/\
                 REFRESH_TOKEN (the refresh token must have been authorized with \
                 user-library-read and playlist-read-private scopes)"
                    .to_string(),
            ),
            Some(status) => {
                ConnectorError::Transient(format!("Spotify token endpoint error {status}"))
            }
            None => ConnectorError::Transient(error.to_string()),
        },
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). The Web API rejects an expired/invalid access token with
/// 401 or 403; everything else non-retryable is a transient upstream
/// problem.
fn classify_api_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => {
                ConnectorError::Auth(format!("Spotify rejected the access token ({status})"))
            }
            Some(status) => ConnectorError::Transient(format!("Spotify API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

fn bool_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<bool>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ConnectorError::Config(format!("sources.<name>.{key} must be a bool, got {v}"))
    })
}

fn ranged_u32_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
    min: u32,
    max: u32,
) -> Result<Option<u32>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    let n = v.as_u64().ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be a positive integer, got {v}"
        ))
    })?;
    if n < min as u64 || n > max as u64 {
        return Err(ConnectorError::Config(format!(
            "sources.<name>.{key} must be between {min} and {max}, got {n}"
        )));
    }
    Ok(Some(n as u32))
}

impl Connector for SpotifyConnector {
    fn type_name(&self) -> &str {
        "spotify"
    }

    fn display_name(&self) -> &str {
        "Spotify"
    }

    fn description(&self) -> &str {
        "Backs up your liked songs and playlist catalog."
    }

    fn docs_url(&self) -> &str {
        "https://developer.spotify.com/documentation/web-api"
    }

    fn secret_keys(&self) -> &[String] {
        &self.secret_keys
    }

    fn wants_managed_http(&self) -> bool {
        true
    }

    fn volatile_fields(&self) -> &[String] {
        &self.volatile_fields
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: true,
            supports_ordered_cursor: true,
            cursor_kind: "timestamp".to_string(),
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: false,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: true,
            ..Capabilities::default()
        }
    }

    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = bool_option(options, "include_liked_tracks")? {
            self.config.include_liked_tracks = v;
        }
        if let Some(v) = bool_option(options, "include_playlists")? {
            self.config.include_playlists = v;
        }
        if let Some(v) = ranged_u32_option(options, "page_size", 1, 50)? {
            self.config.page_size = v;
        }
        Ok(())
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        let Some(http) = ctx.http.as_ref() else {
            out.push(Err(ConnectorError::Config(
                "Spotify connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        let client_id = match ctx.secrets.get("SPOTIFY_CLIENT_ID") {
            Ok(v) => v.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let client_secret = match ctx.secrets.get("SPOTIFY_CLIENT_SECRET") {
            Ok(v) => v.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let refresh = match ctx.secrets.get("SPOTIFY_REFRESH_TOKEN") {
            Ok(v) => v.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let token = match self.access_token(http, &client_id, &client_secret, &refresh) {
            Ok(t) => t,
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let full = ctx.mode == "full" || ctx.mode == "reconcile";
        let mut cursor = ctx
            .cursor
            .as_ref()
            .and_then(|c| c.value.as_object())
            .cloned()
            .unwrap_or_default();
        let mut live_ids = HashSet::new();

        if self.config.include_liked_tracks {
            self.fetch_tracks(http, &token, &mut cursor, full, &mut live_ids, &mut out);
            if matches!(out.last(), Some(Err(_))) {
                return Box::new(out.into_iter());
            }
        }
        if self.config.include_playlists {
            self.fetch_playlists(http, &token, &mut live_ids, &mut out);
            if matches!(out.last(), Some(Err(_))) {
                return Box::new(out.into_iter());
            }
        }

        if full {
            if self.config.include_liked_tracks && self.config.include_playlists {
                out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                    live_ids,
                ))));
            } else {
                // A deliberately-partial enumeration must never offer
                // the skipped kind's stored items up for the sweep.
                // No logger equivalent exists in `RunContext` yet
                // (same gap `github`'s (#86) doc-comment calls out) —
                // stderr stands in.
                eprintln!("spotify: a kind is disabled — deletion detection skipped");
            }
        }

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(
        mode: &str,
        cursor: Option<Value>,
        http: ManagedHttpClient,
        secrets: Option<(&str, &str, &str)>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some((id, secret, refresh)) = secrets {
            store.insert("SPOTIFY_CLIENT_ID".to_string(), id.to_string());
            store.insert("SPOTIFY_CLIENT_SECRET".to_string(), secret.to_string());
            store.insert("SPOTIFY_REFRESH_TOKEN".to_string(), refresh.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "spotify".to_string(),
            cursor: cursor.map(|value| Cursor { value }),
            since: None,
            secrets: Secrets::new(
                store,
                vec![
                    "SPOTIFY_CLIENT_ID".to_string(),
                    "SPOTIFY_CLIENT_SECRET".to_string(),
                    "SPOTIFY_REFRESH_TOKEN".to_string(),
                ],
            ),
            run_id: 1,
            mode: mode.to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: Some(RefCell::new(http)),
        }
    }

    fn no_sleep_client() -> ManagedHttpClient {
        ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), |_| {})
    }

    fn token_body() -> String {
        serde_json::json!({"access_token": "access-tok"}).to_string()
    }

    fn track_entry(id: &str, name: &str, artist: &str, added_at: &str) -> Value {
        serde_json::json!({
            "added_at": added_at,
            "track": {
                "id": id,
                "name": name,
                "artists": [{"name": artist}],
                "external_urls": {"spotify": format!("https://open.spotify.com/track/{id}")},
                "album": {"name": "An Album"},
            }
        })
    }

    fn playlist_entry(id: &str, name: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "description": "a playlist",
            "external_urls": {"spotify": format!("https://open.spotify.com/playlist/{id}")},
        })
    }

    fn page(items: Vec<Value>, next: Option<&str>) -> Value {
        serde_json::json!({"items": items, "next": next})
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    fn connector_for(server_url: &str) -> SpotifyConnector {
        SpotifyConnector::new(SpotifyConfig::default())
            .with_api_base(server_url)
            .with_token_url(format!("{server_url}/api/token"))
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = SpotifyConnector::new(SpotifyConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "spotify".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["SPOTIFY_CLIENT_ID".to_string()]),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: None,
        };
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_without_secrets_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector = connector_for(&server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn a_rejected_token_refresh_is_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/api/token")
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector = connector_for(&server.url());
        let ctx = ctx_with(
            "incremental",
            None,
            no_sleep_client(),
            Some(("id", "secret", "bad-refresh")),
        );
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn full_fetch_yields_tracks_and_playlists_and_a_combined_reconcile_marker() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let tracks = page(
            vec![track_entry(
                "t1",
                "A Song",
                "An Artist",
                "2024-06-01T00:00:00Z",
            )],
            None,
        );
        let playlists = page(vec![playlist_entry("p1", "A Playlist")], None);
        let _m_tracks = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/me/tracks\?.*".to_string()),
            )
            .with_status(200)
            .with_body(tracks.to_string())
            .create();
        let _m_playlists = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/me/playlists\?.*".to_string()),
            )
            .with_status(200)
            .with_body(playlists.to_string())
            .create();

        let mut connector = connector_for(&server.url());
        let ctx = ctx_with(
            "full",
            None,
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let evs = events(connector.fetch(&ctx));

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "{evs:?}");
        let kinds: HashSet<&str> = items.iter().map(|i| i.item_kind.as_str()).collect();
        assert_eq!(kinds, HashSet::from(["track", "playlist"]));

        let track_item = items.iter().find(|i| i.item_kind == "track").unwrap();
        assert_eq!(track_item.title.as_deref(), Some("An Artist — A Song"));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("track:t1") && marker.live_ids.contains("playlist:p1"));
    }

    #[test]
    fn reconcile_with_only_tracks_enabled_withholds_the_reconcile_marker() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let empty = page(vec![], None);
        let _m_tracks = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/tracks.*".to_string()))
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = SpotifyConfig {
            include_playlists: false,
            ..Default::default()
        };
        let mut connector = SpotifyConnector::new(config)
            .with_api_base(server.url())
            .with_token_url(format!("{}/api/token", server.url()));
        let ctx = ctx_with(
            "reconcile",
            None,
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn incremental_tracks_early_stop_past_the_watermark() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let tracks = page(
            vec![
                track_entry("new", "New Song", "Artist", "2024-06-01T00:00:00Z"),
                track_entry("old", "Old Song", "Artist", "2024-01-01T00:00:00Z"),
            ],
            None,
        );
        let _m_tracks = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/tracks.*".to_string()))
            .with_status(200)
            .with_body(tracks.to_string())
            .create();

        let config = SpotifyConfig {
            include_playlists: false,
            ..Default::default()
        };
        let mut connector = SpotifyConnector::new(config)
            .with_api_base(server.url())
            .with_token_url(format!("{}/api/token", server.url()));
        let cursor = serde_json::json!({"tracks_high_watermark": "2024-03-01T00:00:00Z"});
        let ctx = ctx_with(
            "incremental",
            Some(cursor),
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let evs = events(connector.fetch(&ctx));
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id(), "track:new");
    }

    #[test]
    fn a_track_with_no_catalog_id_is_skipped() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let tracks = page(
            vec![serde_json::json!({
                "added_at": "2024-06-01T00:00:00Z",
                "track": {"name": "local file", "artists": []},
            })],
            None,
        );
        let _m_tracks = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/tracks.*".to_string()))
            .with_status(200)
            .with_body(tracks.to_string())
            .create();

        let config = SpotifyConfig {
            include_playlists: false,
            ..Default::default()
        };
        let mut connector = SpotifyConnector::new(config)
            .with_api_base(server.url())
            .with_token_url(format!("{}/api/token", server.url()));
        let ctx = ctx_with(
            "full",
            None,
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|e| matches!(e, FetchEvent::Item(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn a_playlist_with_no_id_is_skipped() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let playlists = page(vec![serde_json::json!({"name": "no id here"})], None);
        let _m_playlists = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/me/playlists.*".to_string()),
            )
            .with_status(200)
            .with_body(playlists.to_string())
            .create();

        let config = SpotifyConfig {
            include_liked_tracks: false,
            ..Default::default()
        };
        let mut connector = SpotifyConnector::new(config)
            .with_api_base(server.url())
            .with_token_url(format!("{}/api/token", server.url()));
        let ctx = ctx_with(
            "full",
            None,
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|e| matches!(e, FetchEvent::Item(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn a_401_from_the_api_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let _m_tracks = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/tracks.*".to_string()))
            .with_status(401)
            .with_body("{}")
            .create();

        let config = SpotifyConfig {
            include_playlists: false,
            ..Default::default()
        };
        let mut connector = SpotifyConnector::new(config)
            .with_api_base(server.url())
            .with_token_url(format!("{}/api/token", server.url()));
        let ctx = ctx_with(
            "incremental",
            None,
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn a_non_auth_error_status_is_classified_as_transient() {
        let mut server = mockito::Server::new();
        let _m_token = server
            .mock("POST", "/api/token")
            .with_status(200)
            .with_body(token_body())
            .create();
        let _m_tracks = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/tracks.*".to_string()))
            .with_status(500)
            .with_body("{}")
            .create();

        let config = SpotifyConfig {
            include_playlists: false,
            ..Default::default()
        };
        let mut connector = SpotifyConnector::new(config)
            .with_api_base(server.url())
            .with_token_url(format!("{}/api/token", server.url()));
        let ctx = ctx_with(
            "incremental",
            None,
            no_sleep_client(),
            Some(("id", "secret", "refresh")),
        );
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Transient(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = SpotifyConnector::new(SpotifyConfig::default());
        assert_eq!(connector.type_name(), "spotify");
        assert_eq!(
            connector.secret_keys(),
            &[
                "SPOTIFY_CLIENT_ID".to_string(),
                "SPOTIFY_CLIENT_SECRET".to_string(),
                "SPOTIFY_REFRESH_TOKEN".to_string(),
            ]
        );
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 2);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_native_deletes);
        assert_eq!(
            connector.volatile_fields(),
            &[
                "track".to_string(),
                "snapshot_id".to_string(),
                "images".to_string(),
                "tracks".to_string(),
            ]
        );
    }

    #[test]
    fn configure_applies_include_liked_tracks_include_playlists_and_page_size_from_options() {
        let mut connector = SpotifyConnector::new(SpotifyConfig::default());
        let options = HashMap::from([
            ("include_liked_tracks".to_string(), serde_json::json!(false)),
            ("include_playlists".to_string(), serde_json::json!(false)),
            ("page_size".to_string(), serde_json::json!(20)),
        ]);
        connector.configure(&options).unwrap();
        assert!(!connector.config.include_liked_tracks);
        assert!(!connector.config.include_playlists);
        assert_eq!(connector.config.page_size, 20);
    }

    #[test]
    fn configure_with_no_matching_keys_leaves_defaults_untouched() {
        let mut connector = SpotifyConnector::new(SpotifyConfig::default());
        connector.configure(&HashMap::new()).unwrap();
        assert!(connector.config.include_liked_tracks);
        assert!(connector.config.include_playlists);
        assert_eq!(connector.config.page_size, 50);
    }

    #[test]
    fn configure_rejects_a_non_bool_include_liked_tracks() {
        let mut connector = SpotifyConnector::new(SpotifyConfig::default());
        let options =
            HashMap::from([("include_liked_tracks".to_string(), serde_json::json!("yes"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_page_size_outside_1_to_50() {
        let mut connector = SpotifyConnector::new(SpotifyConfig::default());
        let options = HashMap::from([("page_size".to_string(), serde_json::json!(51))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }
}
