//! Pinboard connector: backs up your bookmarks (issue #87). Mirrors
//! `dbs.connectors.pinboard` in baileyrd/Daily-Backup-System.
//!
//! The cheapest incremental strategy in this port, because Pinboard
//! hands us a global change signal: `posts/update` returns the
//! account's last-modified timestamp. If it hasn't moved since the
//! stored cursor, the run ends after ONE request — no paging, no
//! hashing, nothing. When it has moved, `posts/all?fromdt=` returns
//! only the posts added/updated since the watermark (minus an
//! overlap; the idempotent upsert dedups it).
//!
//! Identity is Pinboard's own `hash` (md5 of the URL) — stable across
//! edits to title/notes/tags, which is exactly what change detection
//! should catch. `raw` is the verbatim post; nothing on it churns
//! without being a real edit, so there are no volatile fields.
//!
//! Deletion detection requires the full listing (a delta can't see
//! removals): reconcile/full runs page everything (`posts/all` is one
//! response, not actually paginated) and yield one [`ReconcileMarker`].
//!
//! Auth: the API token from Settings → Password, in `PINBOARD_TOKEN`,
//! in Pinboard's `username:HEXTOKEN` form.
//!
//! Etiquette: Pinboard asks for at most one `posts/all` per 5 minutes;
//! this connector calls it at most once per run, and not at all when
//! `posts/update` says nothing changed.
//!
//! Reachable from a real `dbs backup pinboard` run since #164's
//! `dbs-connector-pinboard` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_core::{iso_z, parse_iso};
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.pinboard.in/v1";
const OVERLAP_SECONDS: i64 = 300;

#[derive(Debug, Clone)]
pub struct PinboardConfig {
    pub token_env: String,
}

impl Default for PinboardConfig {
    fn default() -> Self {
        Self {
            token_env: "PINBOARD_TOKEN".to_string(),
        }
    }
}

pub struct PinboardConnector {
    config: PinboardConfig,
    base_url: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
}

impl PinboardConnector {
    pub fn new(config: PinboardConfig) -> Self {
        let token_env = config.token_env.clone();
        Self {
            config,
            base_url: DEFAULT_BASE_URL.to_string(),
            secret_keys: vec![token_env],
            item_kinds: vec![ItemKind {
                name: "bookmark".to_string(),
                display_name: "Bookmark".to_string(),
                description: String::new(),
            }],
        }
    }

    /// Overrides the API base URL (default `https://api.pinboard.in/v1`)
    /// — for tests to point at a local mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn get(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ConnectorError> {
        let url = format!("{}/{method}", self.base_url);
        let mut full_params = vec![
            ("auth_token", token.to_string()),
            ("format", "json".to_string()),
        ];
        full_params.extend(params.iter().cloned());
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, &url, |b| b.query(&full_params))
            .map_err(classify_http_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }
}

/// The `posts/all` query params for a run: empty for a full run (which
/// always re-lists everything, ignoring the cursor) or an incremental
/// run with no stored watermark yet; otherwise `fromdt` set to the
/// watermark minus [`OVERLAP_SECONDS`] (clocks skew and the idempotent
/// upsert dedups the overlap). A pure function so this — the one bit
/// of real request-shaping logic here — is testable without any HTTP
/// fixture.
fn posts_all_params(full: bool, last_update: Option<&str>) -> Vec<(&'static str, String)> {
    if full {
        return Vec::new();
    }
    let Some(last) = last_update else {
        return Vec::new();
    };
    let Some(since) = parse_iso(Some(last)) else {
        return Vec::new();
    };
    vec![(
        "fromdt",
        iso_z(since - chrono::Duration::seconds(OVERLAP_SECONDS)),
    )]
}

fn to_item(post: &Value) -> Option<BackupItem> {
    let ext_id = post
        .get("hash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let created = post
        .get("time")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    let mut item = BackupItem::new(ext_id, "bookmark", post.clone()).ok()?;
    item.title = post
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            post.get("href")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    item.url = post
        .get("href")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.body = post
        .get("extended")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.tags = post
        .get("tags")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    item.created_at = created;
    item.updated_at = None;
    Some(item)
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). Pinboard returns 401 for a rejected token; everything
/// else non-retryable is a transient upstream problem.
fn classify_http_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(401) => ConnectorError::Auth(
                "Pinboard rejected the token (401) — PINBOARD_TOKEN must be the \
                 username:HEXTOKEN value from Settings \u{2192} Password"
                    .to_string(),
            ),
            Some(status) => ConnectorError::Transient(format!("Pinboard API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
    }
}

impl Connector for PinboardConnector {
    fn type_name(&self) -> &str {
        "pinboard"
    }

    fn display_name(&self) -> &str {
        "Pinboard"
    }

    fn description(&self) -> &str {
        "Backs up your Pinboard bookmarks."
    }

    fn docs_url(&self) -> &str {
        "https://pinboard.in/api/"
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
            supports_incremental: true,
            cursor_kind: "timestamp".to_string(),
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: false,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            // posts/all is one (possibly large) response, not paginated.
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
                "Pinboard connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        if !self.secret_keys.contains(&self.config.token_env) {
            out.push(Err(ConnectorError::Config(format!(
                "token_env={:?} must be one of the declared secret_keys {:?}",
                self.config.token_env, self.secret_keys
            ))));
            return Box::new(out.into_iter());
        }
        let token = match ctx.secrets.get(&self.config.token_env) {
            Ok(t) => t.to_string(),
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
        let last_update = cursor
            .get("update_time")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let update_time = match self.get(http, &token, "posts/update", &[]) {
            Ok(v) => v
                .get("update_time")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        if !full {
            if let Some(last) = &last_update {
                if !update_time.is_empty() && &update_time <= last {
                    // Nothing changed since the stored watermark — a
                    // one-request run, no events at all (not even a
                    // checkpoint: the cursor already reflects reality).
                    return Box::new(out.into_iter());
                }
            }
        }

        let params = posts_all_params(full, last_update.as_deref());

        let posts = match self.get(http, &token, "posts/all", &params) {
            Ok(Value::Array(a)) => a,
            Ok(_) => {
                out.push(Err(ConnectorError::Transient(
                    "pinboard: posts/all returned a non-list".to_string(),
                )));
                return Box::new(out.into_iter());
            }
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let mut live_ids = HashSet::new();
        for post in &posts {
            let Some(item) = to_item(post) else {
                continue;
            };
            live_ids.insert(item.external_id().to_string());
            out.push(Ok(FetchEvent::Item(item)));
        }

        if !update_time.is_empty() {
            cursor.insert("update_time".to_string(), Value::String(update_time));
        }
        out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: Value::Object(cursor),
            },
            note: "posts/all done".to_string(),
        })));
        if full {
            out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                live_ids,
            ))));
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
        token: Option<&str>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("PINBOARD_TOKEN".to_string(), t.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "pinboard".to_string(),
            cursor: cursor.map(|value| Cursor { value }),
            since: None,
            secrets: Secrets::new(store, vec!["PINBOARD_TOKEN".to_string()]),
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

    fn post_json(hash: &str, description: &str, href: &str, time: &str, tags: &str) -> Value {
        serde_json::json!({
            "hash": hash,
            "description": description,
            "href": href,
            "time": time,
            "tags": tags,
            "extended": "notes",
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = PinboardConnector::new(PinboardConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "pinboard".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["PINBOARD_TOKEN".to_string()]),
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
    fn fetch_without_a_token_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn nothing_changed_since_the_watermark_is_a_one_request_run_with_no_events() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
            )
            .with_status(200)
            .with_body(r#"{"update_time": "2024-01-01T00:00:00Z"}"#)
            .create();
        // posts/all should never be hit.
        let _m_all = server
            .mock("GET", mockito::Matcher::Regex(r"^/posts/all.*".to_string()))
            .with_status(500)
            .expect(0)
            .create();

        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        let cursor = serde_json::json!({"update_time": "2024-01-01T00:00:00Z"});
        let ctx = ctx_with("incremental", Some(cursor), no_sleep_client(), Some("tok"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(result.is_empty(), "{result:?}");
        _m_all.assert();
    }

    #[test]
    fn something_changed_fetches_posts_all_with_a_fromdt_overlap() {
        let mut server = mockito::Server::new();
        let _m_update = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
            )
            .with_status(200)
            .with_body(r#"{"update_time": "2024-06-01T00:00:00Z"}"#)
            .create();
        let posts = serde_json::json!([post_json(
            "abc123",
            "A post",
            "https://example.com",
            "2024-06-01T00:00:00Z",
            "rust web"
        )]);
        let _m_all = server
            .mock(
                "GET",
                mockito::Matcher::AllOf(vec![
                    mockito::Matcher::Regex(r"^/posts/all\?".to_string()),
                    // 2024-01-01T00:00:00Z minus 300s overlap = 2023-12-31T23:55:00Z
                    mockito::Matcher::Regex(r"fromdt=2023-12-31T23%3A55%3A00Z".to_string()),
                ]),
            )
            .with_status(200)
            .with_body(posts.to_string())
            .create();

        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        let cursor = serde_json::json!({"update_time": "2024-01-01T00:00:00Z"});
        let ctx = ctx_with("incremental", Some(cursor), no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        _m_all.assert();

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id(), "abc123");
        assert_eq!(items[0].tags, vec!["rust".to_string(), "web".to_string()]);
        assert!(!evs
            .iter()
            .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))));
    }

    #[test]
    fn full_fetch_ignores_the_cursor_and_yields_a_reconcile_marker() {
        let mut server = mockito::Server::new();
        let _m_update = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
            )
            .with_status(200)
            .with_body(r#"{"update_time": "2024-06-01T00:00:00Z"}"#)
            .create();
        let posts = serde_json::json!([post_json(
            "abc123",
            "",
            "https://example.com",
            "2024-06-01T00:00:00Z",
            ""
        )]);
        // Absence of `fromdt` on a full run is covered directly by
        // `posts_all_params_is_empty_for_a_full_run_even_with_a_watermark`
        // below (mockito's query matcher can't express a negative
        // assertion), so this just confirms the endpoint is reached.
        let _m_all = server
            .mock("GET", mockito::Matcher::Regex(r"^/posts/all.*".to_string()))
            .with_status(200)
            .with_body(posts.to_string())
            .create();

        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        // A stale cursor that would otherwise short-circuit incremental mode.
        let cursor = serde_json::json!({"update_time": "2099-01-01T00:00:00Z"});
        let ctx = ctx_with("full", Some(cursor), no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        _m_all.assert();

        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, FetchEvent::Item(_)))
                .count(),
            1
        );
        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        assert!(marker.unwrap().live_ids.contains("abc123"));
        // Title falls back to href when description is empty.
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.title.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn a_post_with_no_hash_is_skipped() {
        let mut server = mockito::Server::new();
        let _m_update = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
            )
            .with_status(200)
            .with_body(r#"{"update_time": "2024-06-01T00:00:00Z"}"#)
            .create();
        let posts =
            serde_json::json!([{"description": "no hash here", "href": "https://example.com"}]);
        let _m_all = server
            .mock("GET", mockito::Matcher::Regex(r"^/posts/all.*".to_string()))
            .with_status(200)
            .with_body(posts.to_string())
            .create();

        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|e| matches!(e, FetchEvent::Item(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn a_401_response_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), Some("bad"));
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
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/posts/update.*".to_string()),
            )
            .with_status(500)
            .with_body("{}")
            .create();

        let mut connector =
            PinboardConnector::new(PinboardConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), Some("tok"));
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
        let connector = PinboardConnector::new(PinboardConfig::default());
        assert_eq!(connector.type_name(), "pinboard");
        assert_eq!(connector.secret_keys(), &["PINBOARD_TOKEN".to_string()]);
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 1);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().paginated);
        assert!(!connector.capabilities().supports_native_deletes);
    }

    #[test]
    fn posts_all_params_includes_fromdt_with_overlap_when_incremental_with_a_watermark() {
        let params = posts_all_params(false, Some("2024-01-01T00:00:00Z"));
        assert_eq!(params, vec![("fromdt", "2023-12-31T23:55:00Z".to_string())]);
    }

    #[test]
    fn posts_all_params_is_empty_for_a_full_run_even_with_a_watermark() {
        let params = posts_all_params(true, Some("2024-01-01T00:00:00Z"));
        assert!(params.is_empty(), "{params:?}");
    }

    #[test]
    fn posts_all_params_is_empty_without_a_stored_watermark() {
        let params = posts_all_params(false, None);
        assert!(params.is_empty(), "{params:?}");
    }
}
