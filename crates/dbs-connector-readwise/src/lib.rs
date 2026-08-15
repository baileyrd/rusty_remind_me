//! Readwise connector: backs up your books/articles and highlights
//! (issue #88). Mirrors `dbs.connectors.readwise` in
//! baileyrd/Daily-Backup-System.
//!
//! The cleanest delta of the connector set so far: both v2 list
//! endpoints (`/books/`, `/highlights/`) accept `updated__gt=<ISO>`,
//! so incremental runs are genuine server-side queries against a
//! per-kind watermark (minus an overlap the idempotent upsert dedups)
//! — unlike `github`'s stars, which have no server-side filter and
//! rely on a client-side early-stop. Pagination follows the standard
//! `{"count", "next", "results"}` shape via the server's own `next`
//! URL rather than reconstructing page numbers, so a change to the
//! server's pagination internals can't desync this connector.
//!
//! Identity is `book:<id>` / `highlight:<id>` (Readwise ids are
//! stable). `raw` is the verbatim API object; Readwise reports a real
//! `updated` timestamp per record and no churny counters, so no
//! `volatile_fields` are needed.
//!
//! Deletion detection: full/reconcile runs enumerate both kinds fully
//! and yield one [`ReconcileMarker`]; if either kind is disabled the
//! marker is withheld — a deliberately-partial enumeration must never
//! sweep.
//!
//! Auth: the access token from readwise.io/access_token in
//! `READWISE_TOKEN` (sent as `Authorization: Token <token>`).
//!
//! Reachable from a real `dbs backup readwise` run since #164's
//! `dbs-connector-readwise` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://readwise.io/api/v2";
/// Re-fetch window below the stored watermark, matching `raindrop` and
/// `github`: clocks skew; the idempotent upsert dedups the overlap.
const OVERLAP_SECONDS: i64 = 300;

#[derive(Debug, Clone)]
pub struct ReadwiseConfig {
    pub include_books: bool,
    pub include_highlights: bool,
    /// API page size, max 1000.
    pub page_size: u32,
    pub token_env: String,
}

impl Default for ReadwiseConfig {
    fn default() -> Self {
        Self {
            include_books: true,
            include_highlights: true,
            page_size: 1000,
            token_env: "READWISE_TOKEN".to_string(),
        }
    }
}

pub struct ReadwiseConnector {
    config: ReadwiseConfig,
    base_url: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
}

impl ReadwiseConnector {
    pub fn new(config: ReadwiseConfig) -> Self {
        let token_env = config.token_env.clone();
        Self {
            config,
            base_url: DEFAULT_BASE_URL.to_string(),
            secret_keys: vec![token_env],
            item_kinds: vec![
                ItemKind {
                    name: "book".to_string(),
                    display_name: "Book / article / source".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "highlight".to_string(),
                    display_name: "Highlight".to_string(),
                    description: String::new(),
                },
            ],
        }
    }

    /// Overrides the API base URL (default `https://readwise.io/api/v2`)
    /// — for tests to point at a local mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn get_json(
        &self,
        http: &RefCell<ManagedHttpClient>,
        url: &str,
        token: &str,
        params: Option<&[(&str, String)]>,
    ) -> Result<Value, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, url, |b| {
                let b = b.header("Authorization", format!("Token {token}"));
                match params {
                    Some(p) => b.query(p),
                    None => b,
                }
            })
            .map_err(classify_http_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_kind(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        endpoint: &str,
        kind: &str,
        cursor_key: &str,
        cursor: &mut serde_json::Map<String, Value>,
        full: bool,
        live_ids: &mut HashSet<String>,
        out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    ) {
        let watermark = cursor
            .get(cursor_key)
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let mut max_seen = watermark.clone();
        let mut params = vec![("page_size", self.config.page_size.to_string())];
        if !full {
            if let Some(since) = watermark.as_deref().and_then(|w| parse_iso(Some(w))) {
                params.push((
                    "updated__gt",
                    dbs_core::iso_z(since - chrono::Duration::seconds(OVERLAP_SECONDS)),
                ));
            }
        }

        let mut url = format!("{}/{endpoint}/", self.base_url);
        let mut page = 1u32;
        loop {
            let is_first = page == 1;
            let payload = match self.get_json(http, &url, token, is_first.then_some(&params[..])) {
                Ok(p) => p,
                Err(e) => {
                    out.push(Err(e));
                    return;
                }
            };
            let Some(results) = payload.get("results").and_then(|v| v.as_array()) else {
                out.push(Err(ConnectorError::Transient(format!(
                    "readwise: {endpoint} returned no results list"
                ))));
                return;
            };
            for rec in results {
                let Some(id) = record_id(rec) else {
                    continue;
                };
                let item = to_item(kind, rec, &id);
                live_ids.insert(item.external_id().to_string());
                let updated = rec
                    .get("updated")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !updated.is_empty() && max_seen.as_deref().is_none_or(|m| updated.as_str() > m) {
                    max_seen = Some(updated);
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
                note: format!("{endpoint} page {page}"),
            })));
            match payload.get("next").and_then(|v| v.as_str()) {
                Some(next) if !next.is_empty() => {
                    // The Authorization header is reattached to whatever
                    // `url` becomes on the next request (see get_json) --
                    // a `next` pointing at a different origin than the
                    // configured API host (a compromised/MITM'd response)
                    // would otherwise exfiltrate READWISE_TOKEN there.
                    if !same_origin(&self.base_url, next) {
                        out.push(Err(ConnectorError::Transient(format!(
                            "readwise: refusing to follow a pagination 'next' URL pointing \
                             at a different origin than the configured API host: {next}"
                        ))));
                        return;
                    }
                    url = next.to_string();
                    page += 1;
                }
                _ => break,
            }
        }
        if let Some(seen) = max_seen {
            cursor.insert(cursor_key.to_string(), Value::String(seen));
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: Value::Object(cursor.clone()),
                },
                note: format!("{endpoint} done"),
            })));
        }
    }
}

/// True iff `candidate` parses as a URL sharing `base`'s scheme, host,
/// and (explicit-or-default) port — i.e. the same origin. Used to
/// validate a server-supplied pagination `next` URL before the
/// Authorization header is reattached to it; a malformed or
/// unparseable URL on either side is never same-origin.
fn same_origin(base: &str, candidate: &str) -> bool {
    let (Ok(base), Ok(candidate)) = (reqwest::Url::parse(base), reqwest::Url::parse(candidate))
    else {
        return false;
    };
    base.scheme() == candidate.scheme()
        && base.host_str() == candidate.host_str()
        && base.port_or_known_default() == candidate.port_or_known_default()
}

fn record_id(rec: &Value) -> Option<String> {
    match rec.get("id")? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn tags_from(rec: &Value) -> Vec<String> {
    rec.get("tags")
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

fn to_item(kind: &str, rec: &Value, id: &str) -> BackupItem {
    let mut item = BackupItem::new(format!("{kind}:{id}"), kind, rec.clone())
        .expect("kind-prefixed id is always non-empty");
    if kind == "book" {
        item.title = Some(
            rec.get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("book {id}")),
        );
        item.url = rec
            .get("source_url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                rec.get("highlights_url")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_string);
        item.body = rec
            .get("author")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        item.created_at = rec
            .get("last_highlight_at")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_iso(Some(s)));
    } else {
        let text = rec.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let truncated: String = text.chars().take(120).collect();
        item.title = Some(if truncated.is_empty() {
            format!("highlight {id}")
        } else {
            truncated
        });
        item.url = rec.get("url").and_then(|v| v.as_str()).map(str::to_string);
        item.body = rec.get("text").and_then(|v| v.as_str()).map(str::to_string);
        item.created_at = rec
            .get("highlighted_at")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_iso(Some(s)));
    }
    item.tags = tags_from(rec);
    item.updated_at = rec
        .get("updated")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). Readwise returns 401 or 403 for a rejected token;
/// everything else non-retryable is a transient upstream problem.
fn classify_http_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => ConnectorError::Auth(format!(
                "Readwise rejected the token ({status}) — check READWISE_TOKEN"
            )),
            Some(status) => ConnectorError::Transient(format!("Readwise API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
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

impl Connector for ReadwiseConnector {
    fn type_name(&self) -> &str {
        "readwise"
    }

    fn display_name(&self) -> &str {
        "Readwise"
    }

    fn description(&self) -> &str {
        "Backs up your Readwise books/articles and highlights."
    }

    fn docs_url(&self) -> &str {
        "https://readwise.io/api_deets"
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
            paginated: true,
            ..Capabilities::default()
        }
    }

    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = bool_option(options, "include_books")? {
            self.config.include_books = v;
        }
        if let Some(v) = bool_option(options, "include_highlights")? {
            self.config.include_highlights = v;
        }
        if let Some(v) = ranged_u32_option(options, "page_size", 1, 1000)? {
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
                "Readwise connector requires managed HTTP".to_string(),
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
        let mut live_ids = HashSet::new();

        let kinds: [(bool, &str, &str, &str); 2] = [
            (
                self.config.include_books,
                "books",
                "book",
                "books_high_watermark",
            ),
            (
                self.config.include_highlights,
                "highlights",
                "highlight",
                "highlights_high_watermark",
            ),
        ];
        let mut enabled_count = 0usize;
        for (enabled, endpoint, kind, cursor_key) in kinds {
            if !enabled {
                continue;
            }
            enabled_count += 1;
            self.fetch_kind(
                http,
                &token,
                endpoint,
                kind,
                cursor_key,
                &mut cursor,
                full,
                &mut live_ids,
                &mut out,
            );
            if matches!(out.last(), Some(Err(_))) {
                return Box::new(out.into_iter());
            }
        }

        if full {
            if enabled_count == kinds.len() {
                out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                    live_ids,
                ))));
            } else {
                // A deliberately-partial enumeration must never offer
                // the skipped kind's stored items up for the sweep.
                // No logger equivalent exists in `RunContext` yet
                // (same gap `github`'s (#86) doc-comment calls out) —
                // stderr stands in.
                eprintln!("readwise: a kind is disabled — deletion detection skipped");
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
        token: Option<&str>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("READWISE_TOKEN".to_string(), t.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "readwise".to_string(),
            cursor: cursor.map(|value| Cursor { value }),
            since: None,
            secrets: Secrets::new(store, vec!["READWISE_TOKEN".to_string()]),
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

    fn book_json(id: i64, title: &str, updated: &str) -> Value {
        serde_json::json!({
            "id": id,
            "title": title,
            "source_url": "https://example.com/book",
            "author": "An Author",
            "tags": [{"name": "nonfiction"}],
            "last_highlight_at": updated,
            "updated": updated,
        })
    }

    fn highlight_json(id: i64, text: &str, updated: &str) -> Value {
        serde_json::json!({
            "id": id,
            "text": text,
            "url": "https://example.com/highlight",
            "tags": [{"name": "quote"}],
            "highlighted_at": updated,
            "updated": updated,
        })
    }

    fn page(results: Vec<Value>, next: Option<&str>) -> Value {
        serde_json::json!({
            "count": results.len(),
            "next": next,
            "results": results,
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = ReadwiseConnector::new(ReadwiseConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "readwise".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["READWISE_TOKEN".to_string()]),
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
            ReadwiseConnector::new(ReadwiseConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn full_fetch_yields_books_and_highlights_and_a_combined_reconcile_marker() {
        let mut server = mockito::Server::new();
        let books = page(vec![book_json(1, "A Book", "2024-06-01T00:00:00Z")], None);
        let highlights = page(
            vec![highlight_json(2, "a quote", "2024-06-02T00:00:00Z")],
            None,
        );
        let _m_books = server
            .mock("GET", mockito::Matcher::Regex(r"^/books/\?.*".to_string()))
            .with_status(200)
            .with_body(books.to_string())
            .create();
        let _m_highlights = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/highlights/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(highlights.to_string())
            .create();

        let mut connector =
            ReadwiseConnector::new(ReadwiseConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
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
        assert_eq!(kinds, HashSet::from(["book", "highlight"]));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("book:1") && marker.live_ids.contains("highlight:2"));
    }

    #[test]
    fn reconcile_with_only_books_enabled_withholds_the_reconcile_marker() {
        let mut server = mockito::Server::new();
        let empty = page(vec![], None);
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/books/.*".to_string()))
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("reconcile", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn incremental_sends_updated_gt_with_the_overlap_applied() {
        let mut server = mockito::Server::new();
        let empty = page(vec![], None);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::AllOf(vec![
                    mockito::Matcher::Regex(r"^/books/\?".to_string()),
                    // 2024-01-01T00:00:00Z minus 300s overlap = 2023-12-31T23:55:00Z
                    mockito::Matcher::Regex(r"updated__gt=2023-12-31T23%3A55%3A00Z".to_string()),
                ]),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
        let cursor = serde_json::json!({"books_high_watermark": "2024-01-01T00:00:00Z"});
        let ctx = ctx_with("incremental", Some(cursor), no_sleep_client(), Some("tok"));
        let _ = events(connector.fetch(&ctx));
        _m.assert();
    }

    #[test]
    fn pagination_follows_the_servers_next_url_across_pages() {
        let mut server = mockito::Server::new();
        let page1_next = format!("{}/books/?cursor=abc", server.url());
        let page0 = page(
            vec![book_json(1, "Book One", "2024-06-01T00:00:00Z")],
            Some(&page1_next),
        );
        let page1 = page(vec![book_json(2, "Book Two", "2024-06-02T00:00:00Z")], None);

        let _m0 = server
            .mock("GET", "/books/")
            .match_query(mockito::Matcher::Regex(r"page_size=".to_string()))
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock("GET", "/books/")
            .match_query(mockito::Matcher::Regex(r"cursor=abc".to_string()))
            .with_status(200)
            .with_body(page1.to_string())
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "{evs:?}");
        assert_eq!(items[0].external_id(), "book:1");
        assert_eq!(items[1].external_id(), "book:2");
    }

    #[test]
    fn pagination_refuses_a_next_url_pointing_at_a_different_origin() {
        let mut server = mockito::Server::new();
        // A same-host, different-*origin* (different port) URL is enough
        // to prove the check is real -- it doesn't matter whether the
        // attacker-controlled host is even reachable, since the request
        // must never be attempted at all.
        let cross_origin_next = "http://127.0.0.1:1/books/?cursor=abc";
        let page0 = page(
            vec![book_json(1, "Book One", "2024-06-01T00:00:00Z")],
            Some(cross_origin_next),
        );
        let _m0 = server
            .mock("GET", "/books/")
            .match_query(mockito::Matcher::Regex(r"page_size=".to_string()))
            .with_status(200)
            .with_body(page0.to_string())
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
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
            "https://readwise.io/api/v2",
            "https://readwise.io/api/v2/books/?cursor=abc"
        ));
        assert!(!same_origin(
            "https://readwise.io/api/v2",
            "https://evil.example/books/?cursor=abc"
        ));
        assert!(!same_origin(
            "https://readwise.io/api/v2",
            "http://readwise.io/books/?cursor=abc"
        ));
        assert!(!same_origin(
            "http://127.0.0.1:1234/api/v2",
            "http://127.0.0.1:5678/books/?cursor=abc"
        ));
        assert!(!same_origin("https://readwise.io", "not a url"));
    }

    #[test]
    fn a_book_with_no_title_falls_back_to_a_generated_one() {
        let mut server = mockito::Server::new();
        let mut book = book_json(1, "", "2024-06-01T00:00:00Z");
        book["title"] = Value::String(String::new());
        book["source_url"] = Value::Null;
        book["highlights_url"] = Value::Null;
        let books = page(vec![book], None);
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/books/.*".to_string()))
            .with_status(200)
            .with_body(books.to_string())
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.title.as_deref(), Some("book 1"));
        assert_eq!(item.url, None);
    }

    #[test]
    fn a_highlight_title_truncates_the_text_to_120_chars() {
        let mut server = mockito::Server::new();
        let long_text = "x".repeat(200);
        let highlights = page(
            vec![highlight_json(1, &long_text, "2024-06-01T00:00:00Z")],
            None,
        );
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/highlights/.*".to_string()),
            )
            .with_status(200)
            .with_body(highlights.to_string())
            .create();

        let config = ReadwiseConfig {
            include_books: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.title.as_deref().unwrap().len(), 120);
        assert_eq!(item.body.as_deref(), Some(long_text.as_str()));
    }

    #[test]
    fn a_record_with_no_id_is_skipped() {
        let mut server = mockito::Server::new();
        let books = page(vec![serde_json::json!({"title": "no id here"})], None);
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/books/.*".to_string()))
            .with_status(200)
            .with_body(books.to_string())
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
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
            .mock("GET", mockito::Matcher::Regex(r"^/books/.*".to_string()))
            .with_status(401)
            .with_body("{}")
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
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
            .mock("GET", mockito::Matcher::Regex(r"^/books/.*".to_string()))
            .with_status(500)
            .with_body("{}")
            .create();

        let config = ReadwiseConfig {
            include_highlights: false,
            ..Default::default()
        };
        let mut connector = ReadwiseConnector::new(config).with_base_url(server.url());
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
        let connector = ReadwiseConnector::new(ReadwiseConfig::default());
        assert_eq!(connector.type_name(), "readwise");
        assert_eq!(connector.secret_keys(), &["READWISE_TOKEN".to_string()]);
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 2);
        assert!(connector.capabilities().requires_auth);
        assert!(connector.capabilities().paginated);
        assert!(!connector.capabilities().supports_native_deletes);
    }

    #[test]
    fn configure_applies_include_books_include_highlights_and_page_size_from_options() {
        let mut connector = ReadwiseConnector::new(ReadwiseConfig::default());
        let options = HashMap::from([
            ("include_books".to_string(), serde_json::json!(false)),
            ("include_highlights".to_string(), serde_json::json!(false)),
            ("page_size".to_string(), serde_json::json!(200)),
        ]);
        connector.configure(&options).unwrap();
        assert!(!connector.config.include_books);
        assert!(!connector.config.include_highlights);
        assert_eq!(connector.config.page_size, 200);
    }

    #[test]
    fn configure_with_no_matching_keys_leaves_defaults_untouched() {
        let mut connector = ReadwiseConnector::new(ReadwiseConfig::default());
        connector.configure(&HashMap::new()).unwrap();
        assert!(connector.config.include_books);
        assert!(connector.config.include_highlights);
        assert_eq!(connector.config.page_size, 1000);
    }

    #[test]
    fn configure_rejects_a_non_bool_include_books() {
        let mut connector = ReadwiseConnector::new(ReadwiseConfig::default());
        let options = HashMap::from([("include_books".to_string(), serde_json::json!("yes"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_page_size_outside_1_to_1000() {
        let mut connector = ReadwiseConnector::new(ReadwiseConfig::default());
        let options = HashMap::from([("page_size".to_string(), serde_json::json!(1001))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }
}
