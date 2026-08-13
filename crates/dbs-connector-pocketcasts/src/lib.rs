//! Pocket Casts connector: backs up subscriptions, starred episodes,
//! and listening history (issue #92). Mirrors
//! `dbs.connectors.pocketcasts` in baileyrd/Daily-Backup-System.
//!
//! Pocket Casts has **no official public API**. This connector speaks
//! the reverse-engineered web-player API (the same one the community
//! python/nodejs `pocketcasts` libraries use): a POST to
//! `/user/login` with email/password and `scope: "webplayer"` returns
//! a bearer token, and three POST endpoints list the account's
//! podcast subscriptions, starred episodes, and listening history.
//! Because the API is unofficial it may change without notice — each
//! endpoint call is therefore its own small method so a shift in one
//! endpoint's shape is a one-method fix.
//!
//! Deletion detection: subscriptions and stars *are* full
//! enumerations of your current account state, so every successful
//! complete walk of all three endpoints yields one [`ReconcileMarker`]
//! and unsubscribed podcasts / unstarred episodes get soft-deleted.
//! History entries that scroll off Pocket Casts' server-side history
//! window are soft-deleted too — accepted, not a bug: nothing is
//! lost, there's just visible churn as old history ages out. A
//! deliberately-partial enumeration (any kind disabled) withholds the
//! marker entirely.
//!
//! Change detection: `playedUpTo`/`playingStatus` on history entries
//! churn on every listen; they're declared `volatile_fields` so
//! listening-position micro-updates never spawn revisions.
//!
//! Auth: `POCKETCASTS_EMAIL`/`POCKETCASTS_PASSWORD` (your web-player
//! login).
//!
//! **Not wired up:** same boundary as `dbs-connector-raindrop` (#85),
//! `dbs-connector-github` (#86), `dbs-connector-pinboard` (#87),
//! `dbs-connector-readwise` (#88), `dbs-connector-mastodon` (#89),
//! `dbs-connector-bluesky` (#90), and `dbs-connector-spotify` (#91) —
//! this struct isn't reachable from a real `dbs backup` run yet; the
//! plugin registry's run/stream bridge doesn't exist. Tested directly
//! against the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

const DEFAULT_API_BASE: &str = "https://api.pocketcasts.com";

#[derive(Debug, Clone)]
pub struct PocketCastsConfig {
    pub include_subscriptions: bool,
    pub include_starred: bool,
    pub include_history: bool,
}

impl Default for PocketCastsConfig {
    fn default() -> Self {
        Self {
            include_subscriptions: true,
            include_starred: true,
            include_history: true,
        }
    }
}

pub struct PocketCastsConnector {
    config: PocketCastsConfig,
    api_base: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl PocketCastsConnector {
    pub fn new(config: PocketCastsConfig) -> Self {
        Self {
            config,
            api_base: DEFAULT_API_BASE.to_string(),
            secret_keys: vec![
                "POCKETCASTS_EMAIL".to_string(),
                "POCKETCASTS_PASSWORD".to_string(),
            ],
            item_kinds: vec![
                ItemKind {
                    name: "podcast".to_string(),
                    display_name: "Podcast".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "starred".to_string(),
                    display_name: "Starred episode".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "history".to_string(),
                    display_name: "History entry".to_string(),
                    description: String::new(),
                },
            ],
            // Listening position churns on every play session; without
            // this, each run would spawn a revision per in-progress
            // episode.
            volatile_fields: vec!["playedUpTo".to_string(), "playingStatus".to_string()],
        }
    }

    /// Overrides the API base URL (default
    /// `https://api.pocketcasts.com`) — for tests to point at a local
    /// mock server.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// `POST /user/login` → bearer token for the web-player API.
    fn login(
        &self,
        http: &RefCell<ManagedHttpClient>,
        email: &str,
        password: &str,
    ) -> Result<String, ConnectorError> {
        let body = serde_json::json!({
            "email": email,
            "password": password,
            "scope": "webplayer",
        });
        let response = http
            .borrow_mut()
            .request(
                reqwest::Method::POST,
                &format!("{}/user/login", self.api_base),
                |b| b.json(&body),
            )
            .map_err(classify_login_error)?;
        let payload: Value = response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))?;
        payload
            .get("token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ConnectorError::Auth("Pocket Casts login returned no token".to_string()))
    }

    /// 5xx/timeouts already surface as `Transient` from the managed
    /// client after its retries; only 4xx reaches us as a `Status`.
    fn post_json(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        path: &str,
        body: &Value,
    ) -> Result<Value, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(
                reqwest::Method::POST,
                &format!("{}{path}", self.api_base),
                |b| b.bearer_auth(token).json(body),
            )
            .map_err(classify_api_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    fn list_podcasts(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
    ) -> Result<Vec<Value>, ConnectorError> {
        let payload = self.post_json(
            http,
            token,
            "/user/podcast/list",
            &serde_json::json!({"v": 1}),
        )?;
        records(&payload, "podcasts")
    }

    fn list_starred(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
    ) -> Result<Vec<Value>, ConnectorError> {
        let payload = self.post_json(http, token, "/user/starred", &serde_json::json!({}))?;
        records(&payload, "episodes")
    }

    fn list_history(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
    ) -> Result<Vec<Value>, ConnectorError> {
        let payload = self.post_json(http, token, "/user/history", &serde_json::json!({}))?;
        records(&payload, "episodes")
    }
}

fn records(payload: &Value, key: &str) -> Result<Vec<Value>, ConnectorError> {
    payload
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter(|r| r.is_object()).cloned().collect())
        .ok_or_else(|| {
            ConnectorError::Transient(format!("pocketcasts: response has no {key:?} list"))
        })
}

fn podcast_item(rec: &Value) -> Option<BackupItem> {
    let uuid = rec
        .get("uuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let mut item = BackupItem::new(format!("podcast:{uuid}"), "podcast", rec.clone()).ok()?;
    item.title = rec
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.url = Some(format!("https://pocketcasts.com/podcasts/{uuid}"));
    item.body = rec
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(item)
}

fn episode_item(kind: &str, rec: &Value) -> Option<BackupItem> {
    let uuid = rec
        .get("uuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let podcast_uuid = rec.get("podcastUuid").and_then(|v| v.as_str());
    let url = rec
        .get("shareUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| podcast_uuid.map(|p| format!("https://pocketcasts.com/podcasts/{p}")));
    let mut item = BackupItem::new(format!("{kind}:{uuid}"), kind, rec.clone()).ok()?;
    item.title = rec
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.url = url;
    // Show notes only when the list payload carries them — no extra
    // per-episode calls.
    item.body = rec
        .get("showNotes")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.created_at = rec
        .get("published")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    Some(item)
}

fn classify_login_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(401 | 403) => ConnectorError::Auth(
                "Pocket Casts rejected the login — check POCKETCASTS_EMAIL/POCKETCASTS_PASSWORD"
                    .to_string(),
            ),
            Some(status) => ConnectorError::Transient(format!("Pocket Casts login error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
    }
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). The web-player API rejects an expired/invalid token with
/// 401 or 403; everything else non-retryable is a transient upstream
/// problem.
fn classify_api_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => {
                ConnectorError::Auth(format!("Pocket Casts rejected the token ({status})"))
            }
            Some(status) => ConnectorError::Transient(format!("Pocket Casts API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
    }
}

impl Connector for PocketCastsConnector {
    fn type_name(&self) -> &str {
        "pocketcasts"
    }

    fn display_name(&self) -> &str {
        "Pocket Casts"
    }

    fn description(&self) -> &str {
        "Backs up your Pocket Casts subscriptions, starred episodes, and listening history."
    }

    fn setup_hint(&self) -> &str {
        "Set POCKETCASTS_EMAIL and POCKETCASTS_PASSWORD (your web-player login)."
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
            // The API has no trustworthy since-filter, so every run
            // is a full walk.
            supports_incremental: false,
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: false,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: false,
            ..Capabilities::default()
        }
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        let Some(http) = ctx.http.as_ref() else {
            out.push(Err(ConnectorError::Config(
                "Pocket Casts connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        let email = match ctx.secrets.get("POCKETCASTS_EMAIL") {
            Ok(v) => v.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let password = match ctx.secrets.get("POCKETCASTS_PASSWORD") {
            Ok(v) => v.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let token = match self.login(http, &email, &password) {
            Ok(t) => t,
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let mut live_ids = HashSet::new();
        let mut enabled = 0usize;

        if self.config.include_subscriptions {
            enabled += 1;
            let recs = match self.list_podcasts(http, &token) {
                Ok(r) => r,
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };
            for rec in &recs {
                let Some(item) = podcast_item(rec) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(serde_json::Map::new()),
                },
                note: "subscriptions done".to_string(),
            })));
        }
        if self.config.include_starred {
            enabled += 1;
            let recs = match self.list_starred(http, &token) {
                Ok(r) => r,
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };
            for rec in &recs {
                let Some(item) = episode_item("starred", rec) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(serde_json::Map::new()),
                },
                note: "starred done".to_string(),
            })));
        }
        if self.config.include_history {
            enabled += 1;
            let recs = match self.list_history(http, &token) {
                Ok(r) => r,
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };
            for rec in &recs {
                let Some(item) = episode_item("history", rec) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(serde_json::Map::new()),
                },
                note: "history done".to_string(),
            })));
        }

        // Only a walk of *all* kinds may sweep — a deliberately-partial
        // enumeration would falsely delete the disabled kinds' items.
        if enabled == self.item_kinds.len() {
            out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                live_ids,
            ))));
        } else {
            eprintln!("pocketcasts: a kind is disabled — deletion detection skipped");
        }

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(http: ManagedHttpClient, creds: Option<(&str, &str)>) -> RunContext {
        let mut store = HashMap::new();
        if let Some((email, password)) = creds {
            store.insert("POCKETCASTS_EMAIL".to_string(), email.to_string());
            store.insert("POCKETCASTS_PASSWORD".to_string(), password.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "pocketcasts".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(
                store,
                vec![
                    "POCKETCASTS_EMAIL".to_string(),
                    "POCKETCASTS_PASSWORD".to_string(),
                ],
            ),
            run_id: 1,
            mode: "incremental".to_string(),
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

    fn login_body() -> String {
        serde_json::json!({"token": "bearer-tok"}).to_string()
    }

    fn podcast_json(uuid: &str, title: &str) -> Value {
        serde_json::json!({"uuid": uuid, "title": title, "description": "a podcast"})
    }

    fn episode_json(uuid: &str, title: &str, published: &str) -> Value {
        serde_json::json!({
            "uuid": uuid,
            "title": title,
            "published": published,
            "shareUrl": format!("https://pca.st/episode/{uuid}"),
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = PocketCastsConnector::new(PocketCastsConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "pocketcasts".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["POCKETCASTS_EMAIL".to_string()]),
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
    fn fetch_without_credentials_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector =
            PocketCastsConnector::new(PocketCastsConfig::default()).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn a_rejected_login_is_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/user/login")
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector =
            PocketCastsConnector::new(PocketCastsConfig::default()).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "bad-password")));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn full_fetch_yields_all_kinds_and_a_reconcile_marker() {
        let mut server = mockito::Server::new();
        let _m_login = server
            .mock("POST", "/user/login")
            .with_status(200)
            .with_body(login_body())
            .create();
        let podcasts = serde_json::json!({"podcasts": [podcast_json("p1", "A Podcast")]});
        let starred = serde_json::json!({
            "episodes": [episode_json("s1", "Starred Ep", "2024-06-01T00:00:00Z")]
        });
        let history = serde_json::json!({
            "episodes": [episode_json("h1", "History Ep", "2024-06-02T00:00:00Z")]
        });
        let _m_podcasts = server
            .mock("POST", "/user/podcast/list")
            .with_status(200)
            .with_body(podcasts.to_string())
            .create();
        let _m_starred = server
            .mock("POST", "/user/starred")
            .with_status(200)
            .with_body(starred.to_string())
            .create();
        let _m_history = server
            .mock("POST", "/user/history")
            .with_status(200)
            .with_body(history.to_string())
            .create();

        let mut connector =
            PocketCastsConnector::new(PocketCastsConfig::default()).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "password")));
        let evs = events(connector.fetch(&ctx));

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 3, "{evs:?}");
        let kinds: HashSet<&str> = items.iter().map(|i| i.item_kind.as_str()).collect();
        assert_eq!(kinds, HashSet::from(["podcast", "starred", "history"]));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(
            marker.live_ids.contains("podcast:p1")
                && marker.live_ids.contains("starred:s1")
                && marker.live_ids.contains("history:h1")
        );
    }

    #[test]
    fn a_disabled_kind_withholds_the_reconcile_marker() {
        let mut server = mockito::Server::new();
        let _m_login = server
            .mock("POST", "/user/login")
            .with_status(200)
            .with_body(login_body())
            .create();
        let empty = serde_json::json!({"podcasts": []});
        let _m_podcasts = server
            .mock("POST", "/user/podcast/list")
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = PocketCastsConfig {
            include_starred: false,
            include_history: false,
            ..Default::default()
        };
        let mut connector = PocketCastsConnector::new(config).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "password")));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn a_podcast_with_no_uuid_is_skipped() {
        let mut server = mockito::Server::new();
        let _m_login = server
            .mock("POST", "/user/login")
            .with_status(200)
            .with_body(login_body())
            .create();
        let podcasts = serde_json::json!({"podcasts": [{"title": "no uuid here"}]});
        let _m_podcasts = server
            .mock("POST", "/user/podcast/list")
            .with_status(200)
            .with_body(podcasts.to_string())
            .create();

        let config = PocketCastsConfig {
            include_starred: false,
            include_history: false,
            ..Default::default()
        };
        let mut connector = PocketCastsConnector::new(config).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "password")));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|e| matches!(e, FetchEvent::Item(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn an_episode_url_falls_back_to_the_podcast_page_without_a_share_url() {
        let mut server = mockito::Server::new();
        let _m_login = server
            .mock("POST", "/user/login")
            .with_status(200)
            .with_body(login_body())
            .create();
        let starred = serde_json::json!({
            "episodes": [{
                "uuid": "s1",
                "title": "Starred Ep",
                "podcastUuid": "pod123",
            }]
        });
        let _m_starred = server
            .mock("POST", "/user/starred")
            .with_status(200)
            .with_body(starred.to_string())
            .create();

        let config = PocketCastsConfig {
            include_subscriptions: false,
            include_history: false,
            ..Default::default()
        };
        let mut connector = PocketCastsConnector::new(config).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "password")));
        let evs = events(connector.fetch(&ctx));
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            item.url.as_deref(),
            Some("https://pocketcasts.com/podcasts/pod123")
        );
    }

    #[test]
    fn a_401_from_the_api_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m_login = server
            .mock("POST", "/user/login")
            .with_status(200)
            .with_body(login_body())
            .create();
        let _m_podcasts = server
            .mock("POST", "/user/podcast/list")
            .with_status(401)
            .with_body("{}")
            .create();

        let config = PocketCastsConfig {
            include_starred: false,
            include_history: false,
            ..Default::default()
        };
        let mut connector = PocketCastsConnector::new(config).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "password")));
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
        let _m_login = server
            .mock("POST", "/user/login")
            .with_status(200)
            .with_body(login_body())
            .create();
        let _m_podcasts = server
            .mock("POST", "/user/podcast/list")
            .with_status(500)
            .with_body("{}")
            .create();

        let config = PocketCastsConfig {
            include_starred: false,
            include_history: false,
            ..Default::default()
        };
        let mut connector = PocketCastsConnector::new(config).with_api_base(server.url());
        let ctx = ctx_with(no_sleep_client(), Some(("me@example.com", "password")));
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
        let connector = PocketCastsConnector::new(PocketCastsConfig::default());
        assert_eq!(connector.type_name(), "pocketcasts");
        assert_eq!(
            connector.secret_keys(),
            &[
                "POCKETCASTS_EMAIL".to_string(),
                "POCKETCASTS_PASSWORD".to_string()
            ]
        );
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 3);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(!connector.capabilities().supports_native_deletes);
        assert_eq!(
            connector.volatile_fields(),
            &["playedUpTo".to_string(), "playingStatus".to_string()]
        );
    }
}
