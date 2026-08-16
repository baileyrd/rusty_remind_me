//! Mastodon connector: backs up your bookmarks and favourites (issue
//! #89). Mirrors `dbs.connectors.mastodon` in
//! baileyrd/Daily-Backup-System.
//!
//! Mastodon's bookmark/favourite listings paginate by *internal*
//! marker ids exposed only through `Link` response headers (not
//! status ids), with no usable `since` filter — so, like `reddit`,
//! every run is a full enumeration (`supports_incremental=false`)
//! followed by one [`ReconcileMarker`]. Un-bookmarking something is
//! only visible by its absence, and these lists are human-curated and
//! modest, so a full walk per run stays cheap.
//!
//! Config carries the `instance` base URL (multi-instance accounts =
//! one source each). `raw` is the verbatim status; the top-level
//! engagement counters and the nested `account` object are declared
//! volatile — both churn constantly (boost counts, the author's
//! follower counts) without the *saved* content changing. The author
//! handle is captured into `title` at map time, so meaningful display
//! changes still hash.
//!
//! Auth: an access token in `MASTODON_TOKEN` (Preferences →
//! Development → New application; `read:bookmarks read:favourites`
//! scopes suffice).
//!
//! Reachable from a real `dbs backup mastodon` run since #164's
//! `dbs-connector-mastodon` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct MastodonConfig {
    /// Base URL of the instance, e.g. `https://mastodon.social`.
    pub instance: String,
    pub include_bookmarks: bool,
    pub include_favourites: bool,
    /// API page size, max 40.
    pub page_size: u32,
    pub token_env: String,
}

impl Default for MastodonConfig {
    fn default() -> Self {
        Self {
            instance: String::new(),
            include_bookmarks: true,
            include_favourites: true,
            page_size: 40,
            token_env: "MASTODON_TOKEN".to_string(),
        }
    }
}

pub struct MastodonConnector {
    config: MastodonConfig,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl MastodonConnector {
    pub fn new(config: MastodonConfig) -> Self {
        let token_env = config.token_env.clone();
        Self {
            config,
            secret_keys: vec![token_env],
            item_kinds: vec![
                ItemKind {
                    name: "bookmark".to_string(),
                    display_name: "Bookmarked post".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "favourite".to_string(),
                    display_name: "Favourited post".to_string(),
                    description: String::new(),
                },
            ],
            // Engagement counters and the author object churn without
            // the saved content changing; the author handle is
            // captured into `title` instead.
            volatile_fields: vec![
                "favourites_count".to_string(),
                "reblogs_count".to_string(),
                "replies_count".to_string(),
                "account".to_string(),
            ],
        }
    }

    fn get_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        url: &str,
        token: &str,
        params: Option<&[(&str, String)]>,
    ) -> Result<(Vec<Value>, Option<String>), ConnectorError> {
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, url, |b| {
                let b = b.bearer_auth(token);
                match params {
                    Some(p) => b.query(p),
                    None => b,
                }
            })
            .map_err(classify_http_error)?;
        let next = response
            .headers()
            .get("Link")
            .and_then(|v| v.to_str().ok())
            .and_then(next_link_from_header);
        let value: Value = response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))?;
        let statuses = value.as_array().cloned().ok_or_else(|| {
            ConnectorError::Transient("mastodon: endpoint returned a non-list".to_string())
        })?;
        Ok((statuses, next))
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_kind(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        base: &str,
        endpoint: &str,
        kind: &str,
        live_ids: &mut HashSet<String>,
        out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    ) {
        let mut url = Some(format!("{base}/api/v1/{endpoint}"));
        let mut page = 1u32;
        let mut first = true;

        while let Some(u) = url.take() {
            let params = [("limit", self.config.page_size.to_string())];
            let (statuses, next) =
                match self.get_page(http, &u, token, first.then_some(&params[..])) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(e));
                        return;
                    }
                };
            first = false;
            for status in &statuses {
                let Some(id) = status_id(status) else {
                    continue;
                };
                let item = to_item(kind, status, &id);
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
            }
            // Pagination markers are internal ids surfaced only via
            // the Link header — no cursor of our own to persist since
            // every run walks the full listing from scratch.
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(serde_json::Map::new()),
                },
                note: format!("{endpoint} page {page}"),
            })));
            url = if statuses.is_empty() {
                None
            } else {
                match next {
                    // get_page reattaches the bearer token to whatever
                    // `url` becomes on the next request -- a Link header
                    // pointing at a different origin than the configured
                    // instance (a malicious/compromised instance, since
                    // `instance` is itself arbitrary user-supplied input)
                    // would otherwise exfiltrate MASTODON_TOKEN there.
                    Some(n) if same_origin(base, &n) => Some(n),
                    Some(n) => {
                        out.push(Err(ConnectorError::Transient(format!(
                            "mastodon: refusing to follow a pagination Link header \
                             pointing at a different origin than the configured \
                             instance: {n}"
                        ))));
                        return;
                    }
                    None => None,
                }
            };
            page += 1;
        }
    }
}

/// True iff `candidate` parses as a URL sharing `base`'s scheme, host,
/// and (explicit-or-default) port — i.e. the same origin. Used to
/// validate a Link-header-supplied pagination URL before the bearer
/// token is reattached to it; a malformed or unparseable URL on either
/// side is never same-origin.
fn same_origin(base: &str, candidate: &str) -> bool {
    let (Ok(base), Ok(candidate)) = (reqwest::Url::parse(base), reqwest::Url::parse(candidate))
    else {
        return false;
    };
    base.scheme() == candidate.scheme()
        && base.host_str() == candidate.host_str()
        && base.port_or_known_default() == candidate.port_or_known_default()
}

/// Extracts the `rel="next"` URL from a `Link` header value of the
/// form `<url1>; rel="prev", <url2>; rel="next"`.
fn next_link_from_header(raw: &str) -> Option<String> {
    for part in raw.split(',') {
        let mut segments = part.split(';').map(str::trim);
        let url_part = segments.next()?;
        let url = url_part.strip_prefix('<')?.strip_suffix('>')?;
        for seg in segments {
            if seg == "rel=\"next\"" {
                return Some(url.to_string());
            }
        }
    }
    None
}

fn status_id(status: &Value) -> Option<String> {
    match status.get("id")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn tags_from(status: &Value) -> Vec<String> {
    status
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn to_item(kind: &str, status: &Value, id: &str) -> BackupItem {
    let mut item = BackupItem::new(format!("{kind}:{id}"), kind, status.clone())
        .expect("kind-prefixed id is always non-empty");
    let account = status.get("account").cloned().unwrap_or(Value::Null);
    item.title = account
        .get("acct")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|acct| format!("@{acct}"));
    item.url = status
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            status
                .get("uri")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    item.body = status
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.tags = tags_from(status);
    item.created_at = status
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item.updated_at = status
        .get("edited_at")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). Mastodon returns 401 or 403 for a rejected/under-scoped
/// token; everything else non-retryable is a transient upstream
/// problem.
fn classify_http_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => ConnectorError::Auth(format!(
                "Mastodon rejected the token ({status}) — MASTODON_TOKEN needs \
                 read:bookmarks/read:favourites scopes"
            )),
            Some(status) => ConnectorError::Transient(format!("Mastodon API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

impl Connector for MastodonConnector {
    fn type_name(&self) -> &str {
        "mastodon"
    }

    fn display_name(&self) -> &str {
        "Mastodon"
    }

    fn description(&self) -> &str {
        "Backs up your Mastodon bookmarks and favourites."
    }

    fn docs_url(&self) -> &str {
        "https://docs.joinmastodon.org/methods/bookmarks/"
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

    /// Reads `instance` from this source's `[sources.NAME]` config
    /// (ADR-0002) — there's no sensible default for it (unlike
    /// raindrop/github/etc., which authenticate purely off a secret,
    /// this connector needs to know *which* Mastodon instance to talk
    /// to). Absence isn't an error here: `fetch` already rejects an
    /// empty/non-URL `instance` with a clear `ConnectorError::Config`,
    /// so this only needs to reject the wrong JSON *type* for a value
    /// that is present.
    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = options.get("instance") {
            let instance = v.as_str().ok_or_else(|| {
                ConnectorError::Config(format!("sources.<name>.instance must be a string, got {v}"))
            })?;
            self.config.instance = instance.to_string();
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
                "Mastodon connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        let base = self.config.instance.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            out.push(Err(ConnectorError::Config(format!(
                "instance must be a URL (got {:?})",
                self.config.instance
            ))));
            return Box::new(out.into_iter());
        }
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

        let mut live_ids = HashSet::new();
        let kinds: [(bool, &str, &str); 2] = [
            (self.config.include_bookmarks, "bookmarks", "bookmark"),
            (self.config.include_favourites, "favourites", "favourite"),
        ];
        let mut enabled_count = 0usize;
        for (enabled, endpoint, kind) in kinds {
            if !enabled {
                continue;
            }
            enabled_count += 1;
            self.fetch_kind(http, &token, &base, endpoint, kind, &mut live_ids, &mut out);
            if matches!(out.last(), Some(Err(_))) {
                return Box::new(out.into_iter());
            }
        }

        if enabled_count == kinds.len() {
            out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                live_ids,
            ))));
        } else {
            // A deliberately-partial enumeration must never offer the
            // skipped kind's stored items up for the sweep. No logger
            // equivalent exists in `RunContext` yet (same gap
            // `github`'s (#86) doc-comment calls out) — stderr stands
            // in.
            eprintln!("mastodon: a kind is disabled — deletion detection skipped");
        }

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(http: ManagedHttpClient, token: Option<&str>) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("MASTODON_TOKEN".to_string(), t.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "mastodon".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(store, vec!["MASTODON_TOKEN".to_string()]),
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

    fn status_json(id: &str, acct: &str, content: &str, created_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "url": format!("https://example.social/@{acct}/{id}"),
            "content": content,
            "created_at": created_at,
            "account": {"acct": acct},
            "tags": [{"name": "rust"}],
            "favourites_count": 3,
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    fn config_for(server_url: &str) -> MastodonConfig {
        MastodonConfig {
            instance: server_url.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = MastodonConnector::new(config_for("https://example.social"));
        let ctx = RunContext {
            source_id: 1,
            source_name: "mastodon".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["MASTODON_TOKEN".to_string()]),
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
    fn fetch_with_an_invalid_instance_url_is_a_config_error() {
        let mut connector = MastodonConnector::new(config_for("not-a-url"));
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn configure_applies_a_string_instance_from_options() {
        let mut connector = MastodonConnector::new(MastodonConfig::default());
        assert_eq!(connector.config.instance, "");
        let options = HashMap::from([(
            "instance".to_string(),
            serde_json::json!("https://example.social"),
        )]);
        connector.configure(&options).unwrap();
        assert_eq!(connector.config.instance, "https://example.social");
    }

    #[test]
    fn configure_rejects_a_non_string_instance() {
        let mut connector = MastodonConnector::new(MastodonConfig::default());
        let options = HashMap::from([("instance".to_string(), serde_json::json!(42))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_with_no_instance_key_leaves_the_default_untouched() {
        let mut connector = MastodonConnector::new(MastodonConfig::default());
        connector.configure(&HashMap::new()).unwrap();
        assert_eq!(connector.config.instance, "");
    }

    #[test]
    fn fetch_without_a_token_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector = MastodonConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn full_fetch_yields_bookmarks_and_favourites_and_a_combined_reconcile_marker() {
        let mut server = mockito::Server::new();
        let bookmarks =
            serde_json::json!([status_json("1", "alice", "hello", "2024-06-01T00:00:00Z")]);
        let favourites =
            serde_json::json!([status_json("2", "bob", "world", "2024-06-02T00:00:00Z")]);
        let _m_bookmarks = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
            )
            .with_status(200)
            .with_body(bookmarks.to_string())
            .create();
        let _m_favourites = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/v1/favourites.*".to_string()),
            )
            .with_status(200)
            .with_body(favourites.to_string())
            .create();

        let mut connector = MastodonConnector::new(config_for(&server.url()));
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
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
        assert_eq!(kinds, HashSet::from(["bookmark", "favourite"]));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("bookmark:1") && marker.live_ids.contains("favourite:2"));

        let bookmark_item = items.iter().find(|i| i.item_kind == "bookmark").unwrap();
        assert_eq!(bookmark_item.title.as_deref(), Some("@alice"));
    }

    #[test]
    fn reconcile_with_only_bookmarks_enabled_withholds_the_reconcile_marker() {
        let mut server = mockito::Server::new();
        let empty = serde_json::json!([]);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn pagination_follows_the_link_header_across_pages() {
        let mut server = mockito::Server::new();
        let next_url = format!("{}/api/v1/bookmarks?max_id=abc", server.url());
        let page0 = serde_json::json!([status_json("1", "alice", "p1", "2024-06-01T00:00:00Z")]);
        let page1 = serde_json::json!([status_json("2", "alice", "p2", "2024-06-02T00:00:00Z")]);

        let _m0 = server
            .mock("GET", "/api/v1/bookmarks")
            .match_query(mockito::Matcher::Regex(r"limit=".to_string()))
            .with_status(200)
            .with_header("Link", &format!("<{next_url}>; rel=\"next\""))
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock("GET", "/api/v1/bookmarks")
            .match_query(mockito::Matcher::Regex(r"max_id=abc".to_string()))
            .with_status(200)
            .with_body(page1.to_string())
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "{evs:?}");
        assert_eq!(items[0].external_id(), "bookmark:1");
        assert_eq!(items[1].external_id(), "bookmark:2");
    }

    #[test]
    fn pagination_refuses_a_link_header_pointing_at_a_different_origin() {
        let mut server = mockito::Server::new();
        // A same-host, different-port URL is enough to prove the check is
        // real -- the attacker-controlled host doesn't need to be
        // reachable, since the request must never be attempted at all.
        let cross_origin_next = "http://127.0.0.1:1/api/v1/bookmarks?max_id=abc";
        let page0 = serde_json::json!([status_json("1", "alice", "p1", "2024-06-01T00:00:00Z")]);

        let _m0 = server
            .mock("GET", "/api/v1/bookmarks")
            .match_query(mockito::Matcher::Regex(r"limit=".to_string()))
            .with_status(200)
            .with_header("Link", &format!("<{cross_origin_next}>; rel=\"next\""))
            .with_body(page0.to_string())
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let evs: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            evs.iter().any(|e| matches!(
                e,
                Err(ConnectorError::Transient(m)) if m.contains("different origin")
            )),
            "{evs:?}"
        );
    }

    #[test]
    fn same_origin_matches_scheme_host_and_port_exactly() {
        assert!(same_origin(
            "https://mastodon.social",
            "https://mastodon.social/api/v1/bookmarks?max_id=abc"
        ));
        assert!(!same_origin(
            "https://mastodon.social",
            "https://evil.example/api/v1/bookmarks?max_id=abc"
        ));
        assert!(!same_origin(
            "https://mastodon.social",
            "http://mastodon.social/api/v1/bookmarks?max_id=abc"
        ));
        assert!(!same_origin(
            "http://127.0.0.1:1234",
            "http://127.0.0.1:5678/api/v1/bookmarks?max_id=abc"
        ));
        assert!(!same_origin("https://mastodon.social", "not a url"));
    }

    #[test]
    fn a_status_with_no_id_is_skipped() {
        let mut server = mockito::Server::new();
        let statuses = serde_json::json!([{"content": "no id here"}]);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
            )
            .with_status(200)
            .with_body(statuses.to_string())
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|e| matches!(e, FetchEvent::Item(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn a_status_with_no_account_has_no_title() {
        let mut server = mockito::Server::new();
        let statuses = serde_json::json!([{
            "id": "1",
            "content": "orphan",
            "created_at": "2024-06-01T00:00:00Z",
        }]);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
            )
            .with_status(200)
            .with_body(statuses.to_string())
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.title, None);
    }

    #[test]
    fn a_401_response_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("bad"));
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
                mockito::Matcher::Regex(r"^/api/v1/bookmarks.*".to_string()),
            )
            .with_status(500)
            .with_body("{}")
            .create();

        let config = MastodonConfig {
            include_favourites: false,
            ..config_for(&server.url())
        };
        let mut connector = MastodonConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), Some("tok"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Transient(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn next_link_from_header_parses_a_multi_link_header() {
        let raw = r#"<https://example.social/prev>; rel="prev", <https://example.social/next>; rel="next""#;
        assert_eq!(
            next_link_from_header(raw),
            Some("https://example.social/next".to_string())
        );
    }

    #[test]
    fn next_link_from_header_returns_none_without_a_next_rel() {
        let raw = r#"<https://example.social/prev>; rel="prev""#;
        assert_eq!(next_link_from_header(raw), None);
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = MastodonConnector::new(config_for("https://example.social"));
        assert_eq!(connector.type_name(), "mastodon");
        assert_eq!(connector.secret_keys(), &["MASTODON_TOKEN".to_string()]);
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 2);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(!connector.capabilities().supports_native_deletes);
        assert_eq!(
            connector.volatile_fields(),
            &[
                "favourites_count".to_string(),
                "reblogs_count".to_string(),
                "replies_count".to_string(),
                "account".to_string(),
            ]
        );
    }
}
