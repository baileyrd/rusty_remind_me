//! Raindrop.io bookmark connector (issue #85) — the first real
//! connector this port implements the [`Connector`] trait for.
//! Mirrors `dbs.connectors.raindrop` in baileyrd/Daily-Backup-System.
//!
//! Raindrop's REST API (v1) has two constraints that shape the whole
//! strategy:
//!
//! * there is **no** `lastUpdate` sort and **no** `since`/modified
//!   filter (sort options are only
//!   `-created`/`created`/`title`/`domain`/`sort`/`score`), and
//! * a normal list response never reports removed items (they move to
//!   the Trash collection `-99`).
//!
//! So a naive "fetch everything modified since X" is impossible.
//! Instead this connector runs in three engine-selected modes:
//!
//! * **incremental** (daily fast path) — pages the collection sorted
//!   by `-created` and early-stops once `created` falls below the
//!   stored high-water mark (minus a small overlap), capturing new
//!   items cheaply; optionally polls Trash (`-99`) for fast same-day
//!   deletion detection.
//! * **reconcile** (periodic) — pages through the whole collection so
//!   the engine re-hashes every item (catching *edits* the fast path
//!   structurally misses) and yields a [`ReconcileMarker`] of all live
//!   ids so the engine soft-deletes anything that vanished upstream.
//! * **full** — like reconcile but ignores the existing cursor (first
//!   run / rebuild).
//!
//! The cursor is opaque to the engine:
//! `{"created_high_watermark": ISO, "trash_high_watermark": ISO}`
//! (`trash_high_watermark` reserved for parity with the cursor shape;
//! trash is paged in full each poll — see [`RaindropConnector::poll_trash`]).
//!
//! **Not ported:** `archive_permanent_copy` (an opt-in, Pro-tier-only
//! feature that opportunistically downloads Raindrop's cached
//! snapshot of a bookmark via a redirect-following, deliberately
//! unauthenticated second request). It's off by default in the
//! reference too and orthogonal to what this issue asks for (token
//! auth + delta/cursor fetch) — a reasonable follow-up once media
//! archiving has its own real story in this port.
//!
//! Reachable from a real `dbs backup raindrop` run since #161's
//! `dbs-connector-raindrop` subprocess binary, discovered through the
//! plugin registry's handshake protocol (`dbs-core::registry`, #45)
//! and driven end to end by the run/stream bridge (#157). Tested
//! directly against the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;

use dbs_core::{iso_z, parse_iso};
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};

const TYPES: [&str; 6] = ["link", "article", "image", "video", "document", "audio"];
const TRASH_COLLECTION: i64 = -99;
const DEFAULT_BASE_URL: &str = "https://api.raindrop.io";

/// Mirrors the reference's `RaindropConfig` (minus `archive_permanent_copy`
/// — see the module doc-comment).
#[derive(Debug, Clone)]
pub struct RaindropConfig {
    /// `0` = all collections except Trash.
    pub collection_id: i64,
    pub nested: bool,
    pub include_types: Vec<String>,
    /// Raindrop's `perpage` max is 50.
    pub page_size: u32,
    pub overlap_seconds: i64,
    pub poll_trash: bool,
    pub token_env: String,
}

impl Default for RaindropConfig {
    fn default() -> Self {
        Self {
            collection_id: 0,
            nested: true,
            include_types: TYPES.iter().map(|s| s.to_string()).collect(),
            page_size: 50,
            overlap_seconds: 300,
            poll_trash: true,
            token_env: "RAINDROP_TOKEN".to_string(),
        }
    }
}

/// Raindrop.io bookmark connector.
pub struct RaindropConnector {
    config: RaindropConfig,
    base_url: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl RaindropConnector {
    pub fn new(config: RaindropConfig) -> Self {
        let token_env = config.token_env.clone();
        Self {
            config,
            base_url: DEFAULT_BASE_URL.to_string(),
            secret_keys: vec![token_env],
            item_kinds: TYPES
                .iter()
                .map(|t| ItemKind {
                    name: t.to_string(),
                    display_name: capitalize(t),
                    description: String::new(),
                })
                .collect(),
            // Stripped from `raw` before hashing so cosmetic/derived churn
            // doesn't create spurious revisions.
            volatile_fields: [
                "lastUpdate",
                "cache",
                "domain",
                "user",
                "broken",
                "sort",
                "creatorRef",
                "_id",
                "__v",
                "removed",
                "reminder",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }

    /// Overrides the API base URL (default `https://api.raindrop.io`) —
    /// for tests to point at a local mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn fetch_collection(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        mut cursor: serde_json::Map<String, serde_json::Value>,
        full: bool,
        out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    ) {
        let created_hw = cursor
            .get("created_high_watermark")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_iso(Some(s)));
        let stop_at = if !full {
            created_hw.map(|hw| hw - chrono::Duration::seconds(self.config.overlap_seconds))
        } else {
            None
        };

        let mut max_created = created_hw;
        let mut live_ids: Option<std::collections::HashSet<String>> =
            full.then(std::collections::HashSet::new);
        let url = format!(
            "{}/rest/v1/raindrops/{}",
            self.base_url, self.config.collection_id
        );
        let mut page = 0u32;
        let mut reached_old = false;

        loop {
            let data = match self.get_page(http, &url, token, page) {
                Ok(d) => d,
                Err(e) => {
                    out.push(Err(e));
                    return;
                }
            };
            let items = data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                break;
            }

            for raw in &items {
                let created = raw
                    .get("created")
                    .and_then(|v| v.as_str())
                    .and_then(|s| parse_iso(Some(s)));
                let ext_id = raw
                    .get("_id")
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                if let Some(live) = live_ids.as_mut() {
                    // Records EVERY upstream id (even ones excluded by
                    // include_types) so the reconcile sweep never deletes
                    // items that still exist upstream but are simply out
                    // of this source's current scope.
                    live.insert(ext_id.clone());
                }
                if !full {
                    if let (Some(stop), Some(created)) = (stop_at, created) {
                        if created < stop {
                            reached_old = true;
                            break;
                        }
                    }
                }
                if max_created.is_none() || created.is_some_and(|c| Some(c) > max_created) {
                    max_created = created;
                }
                match self.to_item(raw, false) {
                    Ok(item) => {
                        if !self.config.include_types.is_empty()
                            && !self.config.include_types.contains(&item.item_kind)
                        {
                            continue;
                        }
                        out.push(Ok(FetchEvent::Item(item)));
                    }
                    Err(e) => out.push(Err(e)),
                }
            }

            if let Some(mc) = max_created {
                cursor.insert(
                    "created_high_watermark".to_string(),
                    serde_json::Value::String(iso_z(mc)),
                );
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: serde_json::Value::Object(cursor.clone()),
                },
                note: format!("collection page {page}"),
            })));

            if reached_old || (items.len() as u32) < self.config.page_size {
                break;
            }
            page += 1;
        }

        if let Some(live) = live_ids {
            out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(live))));
        }
    }

    /// IMPORTANT: a raindrop's `created` is its ORIGINAL creation date,
    /// not the date it was trashed, and the API has no trash-time
    /// sort. So an old bookmark trashed today sorts to the END of the
    /// `-created` trash listing — a created-watermark early-stop would
    /// miss exactly the deletions we care about. Pages the ENTIRE
    /// trash collection every run; trash is bounded (Raindrop empties
    /// it periodically) and re-seeing an already-deleted item is a
    /// cheap idempotent no-op in the engine.
    fn poll_trash(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        cursor: &serde_json::Map<String, serde_json::Value>,
        out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    ) {
        let url = format!("{}/rest/v1/raindrops/{TRASH_COLLECTION}", self.base_url);
        let mut page = 0u32;
        loop {
            let data = match self.get_page(http, &url, token, page) {
                Ok(d) => d,
                Err(e) => {
                    out.push(Err(e));
                    return;
                }
            };
            let items = data
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                break;
            }
            for raw in &items {
                match self.to_item(raw, true) {
                    Ok(item) => out.push(Ok(FetchEvent::Item(item))),
                    Err(e) => out.push(Err(e)),
                }
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: serde_json::Value::Object(cursor.clone()),
                },
                note: format!("trash page {page}"),
            })));
            if (items.len() as u32) < self.config.page_size {
                break;
            }
            page += 1;
        }
    }

    fn get_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        url: &str,
        token: &str,
        page: u32,
    ) -> Result<serde_json::Value, ConnectorError> {
        let page_size = self.config.page_size;
        let nested = self.config.nested;
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, url, |b| {
                b.bearer_auth(token).query(&[
                    ("sort", "-created".to_string()),
                    ("perpage", page_size.to_string()),
                    ("page", page.to_string()),
                    ("nested", nested.to_string()),
                ])
            })
            .map_err(classify_http_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    fn to_item(
        &self,
        raw: &serde_json::Value,
        deleted: bool,
    ) -> Result<BackupItem, ConnectorError> {
        let ext_id = raw
            .get("_id")
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let mut itype = raw
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("link")
            .to_string();
        if !TYPES.contains(&itype.as_str()) {
            itype = "link".to_string();
        }
        let mut item = BackupItem::new(ext_id, itype, raw.clone())
            .map_err(|e| ConnectorError::Contract(e.to_string()))?;
        item.title = raw
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        item.url = raw.get("link").and_then(|v| v.as_str()).map(str::to_string);
        item.body = raw
            .get("note")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| raw.get("excerpt").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        item.tags = raw
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        item.created_at = raw
            .get("created")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_iso(Some(s)));
        item.updated_at = raw
            .get("lastUpdate")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_iso(Some(s)));
        if let Some(cover) = raw
            .get("cover")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            item.media.push(dbs_core::MediaRef {
                url: cover.to_string(),
                kind: "image".to_string(),
                filename: None,
                mime: None,
                data: None,
            });
        }
        item.deleted = deleted;
        Ok(item)
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP status
/// per its domain knowledge — [`dbs_core::HttpError`]'s own
/// doc-comment calls this out explicitly. Raindrop returns 401 for a
/// missing/invalid token; everything else non-retryable is treated as
/// a transient upstream problem (matching the reference, which lets
/// any other status propagate as a generic exception for the engine to
/// classify).
fn classify_http_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(401) | Some(403) => ConnectorError::Auth(error.to_string()),
            _ => ConnectorError::Transient(error.to_string()),
        },
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

fn bool_option(
    options: &std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<bool>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ConnectorError::Config(format!("sources.<name>.{key} must be a bool, got {v}"))
    })
}

fn i64_option(
    options: &std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<i64>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_i64().map(Some).ok_or_else(|| {
        ConnectorError::Config(format!("sources.<name>.{key} must be an integer, got {v}"))
    })
}

fn non_negative_i64_option(
    options: &std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<i64>, ConnectorError> {
    let n = i64_option(options, key)?;
    if let Some(n) = n {
        if n < 0 {
            return Err(ConnectorError::Config(format!(
                "sources.<name>.{key} must be >= 0, got {n}"
            )));
        }
    }
    Ok(n)
}

fn ranged_u32_option(
    options: &std::collections::HashMap<String, serde_json::Value>,
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

fn string_array_option(
    options: &std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<Vec<String>>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    let arr = v.as_array().ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be an array of strings, got {v}"
        ))
    })?;
    let strings = arr
        .iter()
        .map(|entry| {
            entry.as_str().map(str::to_string).ok_or_else(|| {
                ConnectorError::Config(format!(
                    "sources.<name>.{key} entries must be strings, got {entry}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(strings))
}

impl Connector for RaindropConnector {
    fn type_name(&self) -> &str {
        "raindrop"
    }

    fn display_name(&self) -> &str {
        "Raindrop.io"
    }

    fn description(&self) -> &str {
        "Bookmarks/raindrops from raindrop.io via the REST API v1."
    }

    fn docs_url(&self) -> &str {
        "https://developer.raindrop.io/"
    }

    fn setup_hint(&self) -> &str {
        "Create an API token at app.raindrop.io → Settings → Integrations, then set \
         RAINDROP_TOKEN in the API keys tab."
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
            supports_native_deletes: true,
            produces_media: true,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: true,
            ..Capabilities::default()
        }
    }

    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = i64_option(options, "collection_id")? {
            self.config.collection_id = v;
        }
        if let Some(v) = bool_option(options, "nested")? {
            self.config.nested = v;
        }
        if let Some(v) = string_array_option(options, "include_types")? {
            self.config.include_types = v;
        }
        if let Some(v) = ranged_u32_option(options, "page_size", 1, 50)? {
            self.config.page_size = v;
        }
        if let Some(v) = non_negative_i64_option(options, "overlap_seconds")? {
            // fetch_collection later builds chrono::Duration::seconds(v),
            // which panics past chrono's representable range (~9.2e15
            // seconds) -- reject it here as a clean config error instead
            // of letting a large-but-"valid" value crash the connector
            // mid-run.
            if chrono::TimeDelta::try_seconds(v).is_none() {
                return Err(ConnectorError::Config(format!(
                    "sources.<name>.overlap_seconds is too large to represent as a duration, got {v}"
                )));
            }
            self.config.overlap_seconds = v;
        }
        if let Some(v) = bool_option(options, "poll_trash")? {
            self.config.poll_trash = v;
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
                "Raindrop connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        if !self.secret_keys.contains(&self.config.token_env) {
            out.push(Err(ConnectorError::Config(format!(
                "token_env={:?} must be one of the declared secret_keys {:?}; set \
                 RAINDROP_TOKEN in your .env.",
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

        let cursor = ctx
            .cursor
            .as_ref()
            .and_then(|c| c.value.as_object())
            .cloned()
            .unwrap_or_default();
        let full = ctx.mode == "full" || ctx.mode == "reconcile";

        self.fetch_collection(http, &token, cursor.clone(), full, &mut out);

        if self.config.poll_trash && ctx.mode == "incremental" {
            self.poll_trash(http, &token, &cursor, &mut out);
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
        cursor: Option<serde_json::Value>,
        http: ManagedHttpClient,
        token: Option<&str>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("RAINDROP_TOKEN".to_string(), t.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "raindrop".to_string(),
            cursor: cursor.map(|value| Cursor { value }),
            since: None,
            secrets: Secrets::new(store, vec!["RAINDROP_TOKEN".to_string()]),
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

    fn no_sleep_client(base: &str) -> ManagedHttpClient {
        let _ = base;
        ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), |_| {})
    }

    fn raindrop_json(id: &str, created: &str, itype: &str) -> serde_json::Value {
        serde_json::json!({
            "_id": id,
            "type": itype,
            "title": format!("Item {id}"),
            "link": format!("https://example.com/{id}"),
            "created": created,
            "lastUpdate": created,
            "tags": ["a", "b"],
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "raindrop".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["RAINDROP_TOKEN".to_string()]),
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
        let mut server = mockito::Server::new();
        let mut connector =
            RaindropConnector::new(RaindropConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(&server.url()), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
        server.reset();
    }

    #[test]
    fn full_fetch_pages_everything_and_yields_a_reconcile_marker() {
        let mut server = mockito::Server::new();
        let page0 = serde_json::json!({"items": [
            raindrop_json("1", "2024-01-02T00:00:00Z", "link"),
            raindrop_json("2", "2024-01-01T00:00:00Z", "article"),
        ]});
        let empty = serde_json::json!({"items": []});
        let _m0 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*page=0.*".to_string()),
            )
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*page=1.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let mut connector =
            RaindropConnector::new(RaindropConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(
            "full",
            None,
            no_sleep_client(&server.url()),
            Some("secret-token"),
        );
        let evs = events(connector.fetch(&ctx));

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title.as_deref(), Some("Item 1"));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("1") && marker.live_ids.contains("2"));
    }

    #[test]
    fn incremental_fetch_early_stops_once_older_than_the_watermark() {
        let mut server = mockito::Server::new();
        let page0 = serde_json::json!({"items": [
            raindrop_json("new", "2024-06-01T00:00:00Z", "link"),
            raindrop_json("old", "2024-01-01T00:00:00Z", "link"),
        ]});
        let _m0 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*".to_string()),
            )
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        // Incremental mode also polls trash by default (poll_trash=true).
        let empty = serde_json::json!({"items": []});
        let _m_trash = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/-99.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let mut connector =
            RaindropConnector::new(RaindropConfig::default()).with_base_url(server.url());
        let cursor = serde_json::json!({"created_high_watermark": "2024-03-01T00:00:00Z"});
        let ctx = ctx_with(
            "incremental",
            Some(cursor),
            no_sleep_client(&server.url()),
            Some("secret-token"),
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
        assert_eq!(items[0].external_id(), "new");
        // No reconcile marker on an incremental pass.
        assert!(!evs
            .iter()
            .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))));
    }

    #[test]
    fn a_deleted_trashed_item_is_yielded_with_deleted_true() {
        let mut server = mockito::Server::new();
        let empty = serde_json::json!({"items": []});
        let trash = serde_json::json!({"items": [
            raindrop_json("gone", "2024-01-01T00:00:00Z", "link"),
        ]});
        let _m_collection = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();
        let _m_trash = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/-99.*".to_string()),
            )
            .with_status(200)
            .with_body(trash.to_string())
            .create();

        let mut connector =
            RaindropConnector::new(RaindropConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(
            "incremental",
            None,
            no_sleep_client(&server.url()),
            Some("secret-token"),
        );
        let evs = events(connector.fetch(&ctx));
        let item = evs.iter().find_map(|e| match e {
            FetchEvent::Item(i) if i.deleted => Some(i),
            _ => None,
        });
        assert!(item.is_some(), "{evs:?}");
        assert_eq!(item.unwrap().external_id(), "gone");
    }

    #[test]
    fn items_outside_include_types_are_filtered() {
        let mut server = mockito::Server::new();
        let page0 = serde_json::json!({"items": [
            raindrop_json("1", "2024-01-01T00:00:00Z", "audio"),
        ]});
        let empty = serde_json::json!({"items": []});
        let _m0 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*page=0.*".to_string()),
            )
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*page=1.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = RaindropConfig {
            include_types: vec!["link".to_string()],
            ..Default::default()
        };
        let mut connector = RaindropConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with(
            "full",
            None,
            no_sleep_client(&server.url()),
            Some("secret-token"),
        );
        let evs = events(connector.fetch(&ctx));
        let items: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, FetchEvent::Item(_)))
            .collect();
        assert!(items.is_empty(), "{evs:?}");
    }

    #[test]
    fn an_unauthorized_response_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/rest/v1/raindrops/0.*".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector =
            RaindropConnector::new(RaindropConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(
            "incremental",
            None,
            no_sleep_client(&server.url()),
            Some("bad-token"),
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
    fn connector_metadata_matches_the_reference() {
        let connector = RaindropConnector::new(RaindropConfig::default());
        assert_eq!(connector.type_name(), "raindrop");
        assert_eq!(connector.secret_keys(), &["RAINDROP_TOKEN".to_string()]);
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 6);
        assert!(connector.capabilities().requires_auth);
        assert!(connector.capabilities().supports_native_deletes);
    }

    #[test]
    fn configure_applies_every_field_from_options() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let options = HashMap::from([
            ("collection_id".to_string(), serde_json::json!(42)),
            ("nested".to_string(), serde_json::json!(false)),
            (
                "include_types".to_string(),
                serde_json::json!(["link", "article"]),
            ),
            ("page_size".to_string(), serde_json::json!(25)),
            ("overlap_seconds".to_string(), serde_json::json!(60)),
            ("poll_trash".to_string(), serde_json::json!(false)),
        ]);
        connector.configure(&options).unwrap();
        assert_eq!(connector.config.collection_id, 42);
        assert!(!connector.config.nested);
        assert_eq!(
            connector.config.include_types,
            vec!["link".to_string(), "article".to_string()]
        );
        assert_eq!(connector.config.page_size, 25);
        assert_eq!(connector.config.overlap_seconds, 60);
        assert!(!connector.config.poll_trash);
    }

    #[test]
    fn configure_with_no_matching_keys_leaves_defaults_untouched() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let defaults = RaindropConfig::default();
        connector.configure(&HashMap::new()).unwrap();
        assert_eq!(connector.config.collection_id, defaults.collection_id);
        assert_eq!(connector.config.nested, defaults.nested);
        assert_eq!(connector.config.include_types, defaults.include_types);
        assert_eq!(connector.config.page_size, defaults.page_size);
        assert_eq!(connector.config.overlap_seconds, defaults.overlap_seconds);
        assert_eq!(connector.config.poll_trash, defaults.poll_trash);
    }

    #[test]
    fn configure_rejects_a_page_size_outside_1_to_50() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let options = HashMap::from([("page_size".to_string(), serde_json::json!(51))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_negative_overlap_seconds() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let options = HashMap::from([("overlap_seconds".to_string(), serde_json::json!(-1))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_an_overlap_seconds_too_large_for_chrono_to_represent() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        // Non-negative (passes the existing bound) but past chrono's
        // representable seconds range -- must be a clean config error, not
        // a panic when fetch_collection later builds a Duration from it.
        let options = HashMap::from([("overlap_seconds".to_string(), serde_json::json!(i64::MAX))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)), "{err:?}");
        // The rejected value must never have been written into config.
        assert_eq!(
            connector.config.overlap_seconds,
            RaindropConfig::default().overlap_seconds
        );
    }

    #[test]
    fn configure_rejects_a_non_array_include_types() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let options = HashMap::from([("include_types".to_string(), serde_json::json!("link"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_an_include_types_array_with_a_non_string_entry() {
        let mut connector = RaindropConnector::new(RaindropConfig::default());
        let options =
            HashMap::from([("include_types".to_string(), serde_json::json!(["link", 42]))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }
}
