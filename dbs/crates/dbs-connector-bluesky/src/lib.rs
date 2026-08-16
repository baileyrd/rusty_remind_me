//! Bluesky connector: backs up your likes (issue #90). Mirrors
//! `dbs.connectors.bluesky` in baileyrd/Daily-Backup-System.
//!
//! AT Protocol makes this refreshingly direct: your likes are
//! *records in your own repo* (collection `app.bsky.feed.like`),
//! enumerable via `com.atproto.repo.listRecords` with plain cursor
//! pagination — no scraping, no browser. Each record is tiny (a
//! subject reference + timestamp), so every run is a full enumeration
//! (`supports_incremental=false`) followed by one [`ReconcileMarker`];
//! un-liking is visible only by absence.
//!
//! Auth: an **app password** (Settings → App Passwords — never the
//! account password) in `BLUESKY_APP_PASSWORD`, exchanged for a
//! session token via `com.atproto.server.createSession` at the start
//! of each run. The `identifier` (handle or DID) lives in config; the
//! resolved DID from the session is what `listRecords` enumerates, so
//! a handle change never breaks the source.
//!
//! Identity is the record's `at://` URI (immutable). `raw` is the
//! verbatim record; like records never mutate, so no
//! `volatile_fields`. The subject post's web URL is derived
//! (`https://bsky.app/profile/<did>/post/<rkey>`) for the `url`
//! field.
//!
//! Reachable from a real `dbs backup bluesky` run since #164's
//! `dbs-connector-bluesky` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

const LIKE_COLLECTION: &str = "app.bsky.feed.like";

#[derive(Debug, Clone)]
pub struct BlueskyConfig {
    /// Your handle (`name.bsky.social`) or DID.
    pub identifier: String,
    /// PDS/service base URL.
    pub service: String,
    /// `listRecords` page size, max 100.
    pub page_size: u32,
    pub app_password_env: String,
}

impl Default for BlueskyConfig {
    fn default() -> Self {
        Self {
            identifier: String::new(),
            service: "https://bsky.social".to_string(),
            page_size: 100,
            app_password_env: "BLUESKY_APP_PASSWORD".to_string(),
        }
    }
}

pub struct BlueskyConnector {
    config: BlueskyConfig,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
}

impl BlueskyConnector {
    pub fn new(config: BlueskyConfig) -> Self {
        let app_password_env = config.app_password_env.clone();
        Self {
            config,
            secret_keys: vec![app_password_env],
            item_kinds: vec![ItemKind {
                name: "like".to_string(),
                display_name: "Liked post".to_string(),
                description: String::new(),
            }],
        }
    }

    fn create_session(
        &self,
        http: &RefCell<ManagedHttpClient>,
        base: &str,
        identifier: &str,
        password: &str,
    ) -> Result<(String, String), ConnectorError> {
        let response = http
            .borrow_mut()
            .request(
                reqwest::Method::POST,
                &format!("{base}/xrpc/com.atproto.server.createSession"),
                |b| b.json(&serde_json::json!({"identifier": identifier, "password": password})),
            )
            .map_err(classify_session_error)?;
        let payload: Value = response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))?;
        let token = payload
            .get("accessJwt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let did = payload
            .get("did")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match (token, did) {
            (Some(t), Some(d)) => Ok((t, d)),
            _ => Err(ConnectorError::Auth(
                "Bluesky createSession returned no session".to_string(),
            )),
        }
    }

    fn list_records(
        &self,
        http: &RefCell<ManagedHttpClient>,
        base: &str,
        token: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(
                reqwest::Method::GET,
                &format!("{base}/xrpc/com.atproto.repo.listRecords"),
                |b| b.bearer_auth(token).query(params),
            )
            .map_err(classify_list_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }
}

fn derive_bsky_url(subject_uri: &str) -> Option<String> {
    let stripped = subject_uri.strip_prefix("at://").unwrap_or(subject_uri);
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() == 3 && parts[1] == "app.bsky.feed.post" {
        Some(format!(
            "https://bsky.app/profile/{}/post/{}",
            parts[0], parts[2]
        ))
    } else {
        None
    }
}

fn to_item(rec: &Value) -> Option<BackupItem> {
    let uri = rec
        .get("uri")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let value = rec.get("value").cloned().unwrap_or(Value::Null);
    let subject_uri = value
        .get("subject")
        .and_then(|s| s.get("uri"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut item = BackupItem::new(uri, "like", rec.clone()).ok()?;
    item.url = derive_bsky_url(subject_uri);
    item.created_at = value
        .get("createdAt")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    Some(item)
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). `createSession` rejects bad credentials with 400 or 401;
/// everything else non-retryable is a transient upstream problem.
fn classify_session_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(400 | 401) => ConnectorError::Auth(
                "Bluesky rejected the credentials — check identifier and \
                 BLUESKY_APP_PASSWORD (an app password, not the account one)"
                    .to_string(),
            ),
            Some(status) => {
                ConnectorError::Transient(format!("Bluesky createSession error {status}"))
            }
            None => ConnectorError::Transient(error.to_string()),
        },
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

/// `listRecords` rejects an expired/invalid session with 401 or 403;
/// everything else non-retryable is a transient upstream problem.
fn classify_list_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => {
                ConnectorError::Auth(format!("Bluesky rejected the session ({status})"))
            }
            Some(status) => ConnectorError::Transient(format!("Bluesky API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

impl Connector for BlueskyConnector {
    fn type_name(&self) -> &str {
        "bluesky"
    }

    fn display_name(&self) -> &str {
        "Bluesky"
    }

    fn description(&self) -> &str {
        "Backs up your Bluesky likes."
    }

    fn docs_url(&self) -> &str {
        "https://docs.bsky.app/docs/api/com-atproto-repo-list-records"
    }

    fn secret_keys(&self) -> &[String] {
        &self.secret_keys
    }

    fn wants_managed_http(&self) -> bool {
        true
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: false,
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: false,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: true,
            ..Capabilities::default()
        }
    }

    /// Reads `identifier` (the handle or DID to authenticate as) from
    /// this source's `[sources.NAME]` config (ADR-0002). Unlike
    /// `instance`/`feeds` on the mastodon/podcast connectors, an empty
    /// `identifier` doesn't block a run today (nothing validates it
    /// before the `createSession` call), but a real backup still needs
    /// the right one to authenticate as the right account.
    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = options.get("identifier") {
            let identifier = v.as_str().ok_or_else(|| {
                ConnectorError::Config(format!(
                    "sources.<name>.identifier must be a string, got {v}"
                ))
            })?;
            self.config.identifier = identifier.to_string();
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
                "Bluesky connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        if !self.secret_keys.contains(&self.config.app_password_env) {
            out.push(Err(ConnectorError::Config(format!(
                "app_password_env={:?} must be one of the declared secret_keys {:?}",
                self.config.app_password_env, self.secret_keys
            ))));
            return Box::new(out.into_iter());
        }
        let password = match ctx.secrets.get(&self.config.app_password_env) {
            Ok(p) => p.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let base = self.config.service.trim_end_matches('/').to_string();
        let (token, did) =
            match self.create_session(http, &base, &self.config.identifier, &password) {
                Ok(v) => v,
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };

        let mut live_ids = HashSet::new();
        let mut cursor_param: Option<String> = None;
        let mut page = 1u32;
        loop {
            let mut params = vec![
                ("repo", did.clone()),
                ("collection", LIKE_COLLECTION.to_string()),
                ("limit", self.config.page_size.to_string()),
            ];
            if let Some(c) = &cursor_param {
                params.push(("cursor", c.clone()));
            }
            let payload = match self.list_records(http, &base, &token, &params) {
                Ok(p) => p,
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };
            let Some(records) = payload.get("records").and_then(|v| v.as_array()) else {
                out.push(Err(ConnectorError::Transient(
                    "bluesky: listRecords returned no records".to_string(),
                )));
                return Box::new(out.into_iter());
            };
            for rec in records {
                let Some(item) = to_item(rec) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
            }
            // No cursor of our own to persist between runs — every run
            // walks the full listing from scratch.
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(serde_json::Map::new()),
                },
                note: format!("likes page {page}"),
            })));
            let next_cursor = payload
                .get("cursor")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if records.is_empty() || next_cursor.is_none() {
                break;
            }
            cursor_param = next_cursor;
            page += 1;
        }

        out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
            live_ids,
        ))));

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(http: ManagedHttpClient, password: Option<&str>) -> RunContext {
        let mut store = HashMap::new();
        if let Some(p) = password {
            store.insert("BLUESKY_APP_PASSWORD".to_string(), p.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "bluesky".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(store, vec!["BLUESKY_APP_PASSWORD".to_string()]),
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

    fn config_for(server_url: &str) -> BlueskyConfig {
        BlueskyConfig {
            identifier: "alice.bsky.social".to_string(),
            service: server_url.to_string(),
            ..Default::default()
        }
    }

    fn session_body() -> String {
        serde_json::json!({"accessJwt": "jwt-token", "did": "did:plc:abc123"}).to_string()
    }

    fn like_record(uri: &str, subject_uri: &str, created_at: &str) -> Value {
        serde_json::json!({
            "uri": uri,
            "value": {
                "subject": {"uri": subject_uri},
                "createdAt": created_at,
            }
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn configure_applies_a_string_identifier_from_options() {
        let mut connector = BlueskyConnector::new(BlueskyConfig::default());
        assert_eq!(connector.config.identifier, "");
        let options = HashMap::from([(
            "identifier".to_string(),
            serde_json::json!("alice.bsky.social"),
        )]);
        connector.configure(&options).unwrap();
        assert_eq!(connector.config.identifier, "alice.bsky.social");
    }

    #[test]
    fn configure_rejects_a_non_string_identifier() {
        let mut connector = BlueskyConnector::new(BlueskyConfig::default());
        let options = HashMap::from([("identifier".to_string(), serde_json::json!(42))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = BlueskyConnector::new(config_for("https://bsky.social"));
        let ctx = RunContext {
            source_id: 1,
            source_name: "bluesky".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["BLUESKY_APP_PASSWORD".to_string()]),
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
    fn fetch_without_an_app_password_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn a_rejected_session_is_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("bad-password"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn full_fetch_yields_likes_and_a_reconcile_marker() {
        let mut server = mockito::Server::new();
        let _m_session = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(200)
            .with_body(session_body())
            .create();
        let records = serde_json::json!({
            "records": [like_record(
                "at://did:plc:abc123/app.bsky.feed.like/xyz",
                "at://did:plc:other/app.bsky.feed.post/rkey1",
                "2024-06-01T00:00:00Z",
            )],
        });
        let _m_list = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.repo.listRecords.*".to_string()),
            )
            .with_status(200)
            .with_body(records.to_string())
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("app-password"));
        let evs = events(connector.fetch(&ctx));

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 1, "{evs:?}");
        assert_eq!(
            items[0].external_id(),
            "at://did:plc:abc123/app.bsky.feed.like/xyz"
        );
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://bsky.app/profile/did:plc:other/post/rkey1")
        );

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        assert!(marker
            .unwrap()
            .live_ids
            .contains("at://did:plc:abc123/app.bsky.feed.like/xyz"));
    }

    #[test]
    fn pagination_follows_the_returned_cursor_across_pages() {
        let mut server = mockito::Server::new();
        let _m_session = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(200)
            .with_body(session_body())
            .create();
        let page0 = serde_json::json!({
            "records": [like_record(
                "at://did:plc:abc123/app.bsky.feed.like/1",
                "at://did:plc:other/app.bsky.feed.post/r1",
                "2024-06-01T00:00:00Z",
            )],
            "cursor": "next-page-cursor",
        });
        let page1 = serde_json::json!({
            "records": [like_record(
                "at://did:plc:abc123/app.bsky.feed.like/2",
                "at://did:plc:other/app.bsky.feed.post/r2",
                "2024-06-02T00:00:00Z",
            )],
        });
        // `Matcher::Any` on the first page's query: the second mock
        // (registered later, so checked first per mockito's LIFO
        // matching) only matches once `cursor=` is present, so this
        // one is only ever reached for the cursor-less first request.
        let _m0 = server
            .mock("GET", "/xrpc/com.atproto.repo.listRecords")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock("GET", "/xrpc/com.atproto.repo.listRecords")
            .match_query(mockito::Matcher::Regex(
                r"cursor=next-page-cursor".to_string(),
            ))
            .with_status(200)
            .with_body(page1.to_string())
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("app-password"));
        let evs = events(connector.fetch(&ctx));
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "{evs:?}");
    }

    #[test]
    fn a_record_with_no_uri_is_skipped() {
        let mut server = mockito::Server::new();
        let _m_session = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(200)
            .with_body(session_body())
            .create();
        let records = serde_json::json!({"records": [{"value": {"subject": {}}}]});
        let _m_list = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.repo.listRecords.*".to_string()),
            )
            .with_status(200)
            .with_body(records.to_string())
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("app-password"));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|e| matches!(e, FetchEvent::Item(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn a_subject_that_is_not_a_post_has_no_derived_url() {
        let mut server = mockito::Server::new();
        let _m_session = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(200)
            .with_body(session_body())
            .create();
        let records = serde_json::json!({
            "records": [like_record(
                "at://did:plc:abc123/app.bsky.feed.like/1",
                "at://did:plc:other/app.bsky.actor.profile/self",
                "2024-06-01T00:00:00Z",
            )],
        });
        let _m_list = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.repo.listRecords.*".to_string()),
            )
            .with_status(200)
            .with_body(records.to_string())
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("app-password"));
        let evs = events(connector.fetch(&ctx));
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.url, None);
    }

    #[test]
    fn a_401_from_list_records_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m_session = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(200)
            .with_body(session_body())
            .create();
        let _m_list = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.repo.listRecords.*".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("app-password"));
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
        let _m_session = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.server.createSession".to_string()),
            )
            .with_status(200)
            .with_body(session_body())
            .create();
        let _m_list = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/xrpc/com.atproto.repo.listRecords.*".to_string()),
            )
            .with_status(500)
            .with_body("{}")
            .create();

        let mut connector = BlueskyConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("app-password"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Transient(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn derive_bsky_url_handles_a_valid_post_subject() {
        assert_eq!(
            derive_bsky_url("at://did:plc:abc/app.bsky.feed.post/xyz"),
            Some("https://bsky.app/profile/did:plc:abc/post/xyz".to_string())
        );
    }

    #[test]
    fn derive_bsky_url_returns_none_for_an_empty_subject() {
        assert_eq!(derive_bsky_url(""), None);
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = BlueskyConnector::new(config_for("https://bsky.social"));
        assert_eq!(connector.type_name(), "bluesky");
        assert_eq!(
            connector.secret_keys(),
            &["BLUESKY_APP_PASSWORD".to_string()]
        );
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 1);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(!connector.capabilities().supports_native_deletes);
    }
}
