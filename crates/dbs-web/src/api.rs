//! The `/api` route layer (issue #170, first slice of #169's umbrella)
//! — bridges the shipped SPA's `fetch("/api/...")` calls into
//! `dbs-core`'s existing, synchronous `BackupService`/`Storage`/
//! `ConnectorRegistry` APIs. Every handler in this module follows the
//! same shape: clone `AppState::config` (cheap, `Arc`-backed), open a
//! fresh `SqliteStorage` + build a fresh `ConnectorRegistry`/
//! `SubprocessRunner`/`BackupService` inside a [`tokio::task::spawn_blocking`]
//! closure (per `lib.rs`'s module doc-comment: `dbs-core` stays fully
//! synchronous, nothing in it needs to know Tokio exists), and returns
//! the result as JSON.
//!
//! This module covers the read-only dashboard endpoints — `meta`,
//! `status`, `metrics`, `history`, `vpn`, `verify` — the smallest,
//! lowest-risk slice of #169's full `/api` surface, and the one that
//! establishes the async/sync bridging pattern every other slice
//! (#171-#177) reuses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use dbs_core::service::{BackupAllOptions, BackupService, BackupSourceOptions, ProgressSink};
use dbs_core::{
    build_registry, in_named_netns, named_netns_exists, CancelToken, DbsError, ExportQuery,
    ItemRow, ProgressEvent, SqliteStorage, Storage, SubprocessRunner, VpnGuard,
    CURRENT_API_VERSION,
};

use crate::jobs::{Job, JobAlreadyRunning, JobSnapshot};
use crate::AppState;

/// Export formats `dbs export --format` accepts (`dbs-cli/src/main.rs`'s
/// own doc comment on that flag is the source of truth this list
/// mirrors — there's no canonical list exported from `dbs-core` today).
const EXPORT_FORMATS: &[&str] = &[
    "json", "ndjson", "csv", "markdown", "archive", "obsidian", "wiki",
];

/// Every `/api` failure becomes one JSON shape: `{"detail": "..."}"`
/// with a status code chosen from the underlying [`DbsError`] variant.
/// `Config`/`Load`/`Run` map to 400 (the request or its target source
/// is the problem); `Storage`/`Connector` map to 500 (something on
/// this server's side went wrong, not the caller's). The key is
/// `detail`, not `error` — the shipped frontend's `api()`/`apiUpload()`
/// helpers (`app.js`) read `(await res.json()).detail` on a non-OK
/// response (the reference is a FastAPI app; this mirrors
/// `HTTPException`'s default error body shape), so `error` would
/// silently swallow every server-side error message the frontend was
/// built to show, falling back to a bare HTTP status text instead.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"detail": self.1}))).into_response()
    }
}

impl From<DbsError> for ApiError {
    fn from(e: DbsError) -> Self {
        let status = match &e {
            DbsError::Config(_) | DbsError::Load(_) | DbsError::Run(_) => StatusCode::BAD_REQUEST,
            DbsError::Storage(_) | DbsError::Connector(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

/// Opens a fresh `SqliteStorage` against `config.database` and migrates
/// it — the same two calls every `dbs-cli` `cmd_*` function starts
/// with. A fresh connection per request (rather than one shared,
/// `Mutex`-guarded connection in `AppState`) matches how the CLI
/// already treats storage: cheap to open, no long-lived lock a slow
/// request could hold across others.
fn open_storage(config: &dbs_core::Config) -> Result<SqliteStorage, DbsError> {
    let mut storage = SqliteStorage::open(&config.database)?;
    storage.migrate()?;
    Ok(storage)
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/meta", get(meta))
        .route("/api/status", get(status))
        .route("/api/metrics", get(metrics))
        .route("/api/history", get(history))
        .route("/api/vpn", get(vpn))
        .route("/api/verify", get(verify))
        .route("/api/items", get(items))
        .route("/api/items/:id", get(item_detail))
        .route("/api/media/:id", get(media))
        .route("/api/thumb/:id", get(thumb))
        .route("/api/connectors", get(connectors))
        .route("/api/sources", get(sources).post(create_source))
        .route("/api/secrets", get(secrets).post(set_secret))
        .route("/api/secrets/:name", delete(delete_secret))
        .route("/api/backup", post(start_backup))
        .route("/api/backup/current", get(current_backup))
        .route("/api/backup/:id/cancel", post(cancel_backup))
}

async fn meta(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "tool_version": env!("CARGO_PKG_VERSION"),
        "core_api_version": CURRENT_API_VERSION,
        "config_path": state
            .config
            .source_path
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "setup_enabled": state.allow_setup,
        "scheduler_enabled": state.scheduler_enabled,
        "formats": EXPORT_FORMATS,
    }))
}

#[derive(Deserialize)]
struct SourceParam {
    source: Option<String>,
}

async fn status(
    State(state): State<AppState>,
    Query(params): Query<SourceParam>,
) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let rows = service.status(params.source.as_deref())?;
        Ok(serde_json::to_value(rows).unwrap_or(Value::Null))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

async fn metrics(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let row = service.metrics()?;
        Ok(serde_json::to_value(row).unwrap_or(Value::Null))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

#[derive(Deserialize)]
struct HistoryParams {
    source: Option<String>,
    #[serde(default = "default_history_limit")]
    limit: u32,
}

fn default_history_limit() -> u32 {
    25
}

async fn history(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let runs = service.history(params.source.as_deref(), params.limit)?;
        Ok(serde_json::to_value(runs).unwrap_or(Value::Null))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

/// Single aggregate object, not a per-source list — matches exactly
/// what the shipped frontend's `refreshVpn`/`applyVpnUI` (`app.js`)
/// expect: `relevant` (any enabled source is `requires_vpn`), `up`
/// (`false` whenever a through-VPN run would currently fail — either
/// the namespace isn't up at all, or it's up but this process isn't
/// joined to it; both are "fail-closed, disable Run" per the
/// frontend's own comment), and a human `detail` string.
async fn vpn(State(state): State<AppState>) -> Json<Value> {
    let config = &state.config;
    let relevant = config.sources.values().any(|s| s.enabled && s.requires_vpn);
    if !relevant {
        return Json(json!({"relevant": false, "up": null, "detail": ""}));
    }
    if config.vpn_guard == VpnGuard::Off {
        return Json(json!({
            "relevant": true,
            "up": null,
            "detail": "vpn_guard=off (not enforced)",
        }));
    }
    let ns = &config.vpn_netns;
    if in_named_netns(ns) {
        return Json(json!({
            "relevant": true,
            "up": true,
            "detail": format!("running inside the {ns:?} netns"),
        }));
    }
    let detail = if named_netns_exists(ns) {
        format!(
            "the {ns:?} netns is up but this process isn't in it \u{2014} run via `{}`",
            config.vpn_exec
        )
    } else {
        format!(
            "the {ns:?} netns is not up \u{2014} start it (e.g. `sudo systemctl start vpn-netns`), \
             then run via `{}`",
            config.vpn_exec
        )
    };
    Json(json!({"relevant": true, "up": false, "detail": detail}))
}

/// `dbs verify` itself is an unimplemented stub in this port today
/// (`dbs-cli/src/main.rs`'s generic "not yet implemented" fallback —
/// there's no `cmd_verify`, no `BackupService::verify` to bridge to).
/// `/api/verify` reports that honestly rather than inventing behavior
/// the CLI doesn't have yet.
async fn verify() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"detail": "verify is not yet implemented (tracked in a follow-up issue)"})),
    )
        .into_response()
}

// -- items / media (issue #171) ----------------------------------------

/// A minimal `?key=val&key=val` multi-value query parser — `app.js`'s
/// `browseParams()` sends `source`/`type` as repeated keys (one per
/// selected filter chip), which `axum::extract::Query` (built on
/// `serde_urlencoded`) can't collect into a `Vec` the way this needs.
/// Built on `url::form_urlencoded` (already a `dbs-web` dependency for
/// other reasons) rather than pulling in a new query-string crate just
/// for this.
struct MultiQuery(Vec<(String, String)>);

impl MultiQuery {
    fn parse(raw: Option<&str>) -> Self {
        Self(
            url::form_urlencoded::parse(raw.unwrap_or("").as_bytes())
                .into_owned()
                .collect(),
        )
    }

    fn all(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn one(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

/// Mirrors `dbs-cli`'s `parse_date_arg` (`main.rs`), which accepts
/// either a full ISO-8601 timestamp or a bare `YYYY-MM-DD` date — same
/// acceptance rules, translated to a 400 `ApiError` instead of a CLI
/// error string.
fn parse_date_param(
    value: Option<String>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, ApiError> {
    let Some(text) = value else {
        return Ok(None);
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some(dt) = dbs_core::parse_iso(Some(text)) {
        return Ok(Some(dt));
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map(|d| {
            Some(
                d.and_hms_opt(0, 0, 0)
                    .expect("midnight is always a valid time")
                    .and_utc(),
            )
        })
        .map_err(|e| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("invalid date {text:?}: {e}"),
            )
        })
}

/// `GET /api/items` — mirrors `dbs items`' (`cmd_items` in
/// `dbs-cli/src/main.rs`) list branch: `source`/`type` (repeatable),
/// `q` (search text), `since`/`until`, `include_deleted`, `limit`,
/// `offset`. Response envelope (`{items, total, limit, offset}`)
/// matches both the CLI's own `--json` output and what `app.js`'s
/// `loadBrowseCardsFlat`/`loadBrowseTable`/`updateBrowsePager` expect.
async fn items(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let q = MultiQuery::parse(raw.as_deref());
    let sources = q.all("source");
    let item_types = q.all("type");
    let search = q.one("q");
    let since = parse_date_param(q.one("since"))?;
    let until = parse_date_param(q.one("until"))?;
    let include_deleted = q.one("include_deleted").as_deref() == Some("true");
    let limit: u32 = q.one("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let offset: u32 = q.one("offset").and_then(|v| v.parse().ok()).unwrap_or(0);

    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let query = ExportQuery {
            sources: (!sources.is_empty()).then_some(sources),
            item_types: (!item_types.is_empty()).then_some(item_types),
            since,
            until,
            include_deleted,
            ..Default::default()
        };
        let (rows, total) = service.browse_items(&query, search.as_deref(), limit, offset)?;
        Ok(json!({"items": rows, "total": total, "limit": limit, "offset": offset}))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

/// `GET /api/items/:id` — mirrors `dbs items <id>`'s detail branch.
/// Already carries a `media` array (`id`/`filename`/`mime`/`kind`/
/// `byte_size`/`has_data` per entry, from `Storage::get_item`'s own
/// `media_for_item` join) matching `openItemDrawer`'s (`app.js`)
/// expectations exactly — no gap to close here beyond bridging.
async fn item_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Option<ItemRow>, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        service.get_item(id)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(match result {
        Some(row) => Json(row).into_response(),
        None => not_found("no such item"),
    })
}

fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": message}))).into_response()
}

/// Reconstructs the raw bytes `Storage::get_media_blob`'s generic
/// `ItemRow` shape carries as `data`: a JSON array of byte values (from
/// `serde_json::to_value(&Vec<u8>)`, chosen storage-side to keep that
/// method's return type uniform with every other `ItemRow`-returning
/// one) — converted back here rather than adding a raw-bytes-typed
/// `Storage` method just for this one caller.
fn bytes_from_blob_row(row: &ItemRow) -> Vec<u8> {
    row.get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_u64)
                .map(|b| b as u8)
                .collect()
        })
        .unwrap_or_default()
}

fn mime_from_blob_row(row: &ItemRow) -> String {
    row.get("mime")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// `GET /api/media/:id` — a real binary response (not JSON), matching
/// how `app.js` uses it directly as an `<img src>`/download `<a href>`
/// (`openItemDrawer`'s media list).
async fn media(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Option<ItemRow>, DbsError> {
        let storage = open_storage(&config)?;
        storage.get_media_blob(id)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(match result {
        Some(row) => {
            let mime = mime_from_blob_row(&row);
            let data = bytes_from_blob_row(&row);
            (StatusCode::OK, [(header::CONTENT_TYPE, mime)], data).into_response()
        }
        None => not_found("no such media"),
    })
}

/// The 11-character YouTube video id from a `?v=` query parameter —
/// mirrors `app.js`'s own `/[?&]v=[\w-]{11}/` detection (`thumbUrl`),
/// just via a real URL parse instead of a regex.
fn extract_youtube_video_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "v")
        .map(|(_, v)| v.into_owned())
        .filter(|v| v.len() == 11)
}

enum ThumbOutcome {
    Media { data: Vec<u8>, mime: String },
    Redirect(String),
    NotFound,
}

/// `GET /api/thumb/:id` — `:id` is an *item* id, not a media id (see
/// `app.js`'s `thumbUrl(it)`: `withToken(\`/api/thumb/${it.id}\`)`).
/// Serves the item's own locally-stored image media if it has one
/// (same bytes `/api/media/:id` would, just resolved from the item
/// side); otherwise, for a YouTube item whose URL carries a `v=`
/// video id, redirects to YouTube's public thumbnail CDN rather than
/// proxying bytes this server never stored (YouTube connector items
/// have no image media rows at all — `app.js`'s own comment on this).
async fn thumb(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let config = state.config.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<ThumbOutcome, DbsError> {
        let mut storage = open_storage(&config)?;
        let item = {
            let (registry, _report) = build_registry(&config);
            let runner = SubprocessRunner::new(&config);
            let service = BackupService::new(&mut storage, &config, &registry, &runner);
            service.get_item(id)?
        };
        let Some(item) = item else {
            return Ok(ThumbOutcome::NotFound);
        };

        let image_media_id = item
            .get("media")
            .and_then(Value::as_array)
            .and_then(|media| {
                media.iter().find(|m| {
                    m.get("has_data").and_then(Value::as_bool).unwrap_or(false)
                        && m.get("mime")
                            .and_then(Value::as_str)
                            .is_some_and(|mime| mime.starts_with("image/"))
                })
            })
            .and_then(|m| m.get("id"))
            .and_then(Value::as_i64);
        if let Some(media_id) = image_media_id {
            if let Some(blob) = storage.get_media_blob(media_id)? {
                return Ok(ThumbOutcome::Media {
                    data: bytes_from_blob_row(&blob),
                    mime: mime_from_blob_row(&blob),
                });
            }
        }

        if item.get("type").and_then(Value::as_str) == Some("youtube") {
            if let Some(video_id) = item
                .get("url")
                .and_then(Value::as_str)
                .and_then(extract_youtube_video_id)
            {
                return Ok(ThumbOutcome::Redirect(format!(
                    "https://img.youtube.com/vi/{video_id}/mqdefault.jpg"
                )));
            }
        }
        Ok(ThumbOutcome::NotFound)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;

    Ok(match outcome {
        ThumbOutcome::Media { data, mime } => {
            (StatusCode::OK, [(header::CONTENT_TYPE, mime)], data).into_response()
        }
        ThumbOutcome::Redirect(url) => Redirect::temporary(&url).into_response(),
        ThumbOutcome::NotFound => not_found("no thumbnail available"),
    })
}

// -- sources / connectors (issue #172) ----------------------------------

/// `GET /api/connectors` — every loadable connector, `type`/`label`/
/// `capabilities`/`secret_keys`/`auth_capture`/... per
/// `BackupService::list_connectors`. `app.js`'s `refreshStatus`,
/// `loadConnectorsPanel`, and `loadAddForm` all fetch this as a flat
/// array (`conns.map((c) => [c.type, c])`, `items.forEach((c) => ...)`),
/// so the `Vec<ConnectorInfo>` is returned as-is rather than wrapped in
/// an envelope.
async fn connectors(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let rows = service.list_connectors();
        Ok(serde_json::to_value(rows).unwrap_or(Value::Null))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

/// `GET /api/sources` — every configured source, `name`/`type`/
/// `enabled`/`schedule`/`backed_up` per `BackupService::list_sources`.
/// `app.js`'s `loadSourceDetail` fetches this as a flat array too.
async fn sources(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let rows = service.list_sources()?;
        Ok(serde_json::to_value(rows).unwrap_or(Value::Null))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

/// Request body for `POST /api/sources` — matches exactly what
/// `app.js`'s add-source form submit sends (`loadAddForm`'s submit
/// handler): `name`/`type`/`options`/`store_media`/`max_media_mb`/
/// `requires_vpn`.
#[derive(Deserialize)]
struct CreateSourceRequest {
    name: String,
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    options: HashMap<String, Value>,
    #[serde(default)]
    store_media: bool,
    #[serde(default)]
    max_media_mb: u32,
    #[serde(default)]
    requires_vpn: bool,
}

/// `POST /api/sources` — bridges `BackupService::add_source`. On
/// success returns `{"name": ..., "type": ...}`, which is all the
/// frontend's success toast (`Added ${sc.name} (${sc.type})`) reads.
async fn create_source(
    State(state): State<AppState>,
    Json(body): Json<CreateSourceRequest>,
) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let name = body.name.clone();
    let type_ = body.type_.clone();
    tokio::task::spawn_blocking(move || -> Result<(), DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        service.add_source(
            &body.name,
            &body.type_,
            &body.options,
            body.store_media,
            body.max_media_mb,
            body.requires_vpn,
        )
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({"name": name, "type": type_})))
}

// -- secrets (issue #173) ------------------------------------------------

/// `type -> secret_keys` for every registered connector.
type SecretKeysByType = HashMap<String, Vec<String>>;

/// Every secret key any *registered* connector declares (sorted,
/// deduped), alongside a `type -> secret_keys` map used to figure out
/// which configured *sources* need which key. Shared by every `/api/secrets`
/// handler so "is this a real secret key" is answered from one place —
/// the live registry, not a hardcoded list this module would have to
/// keep in sync by hand.
fn allowed_secret_keys(
    config: &dbs_core::Config,
) -> Result<(Vec<String>, SecretKeysByType), DbsError> {
    let mut storage = open_storage(config)?;
    let (registry, _report) = build_registry(config);
    let runner = SubprocessRunner::new(config);
    let service = BackupService::new(&mut storage, config, &registry, &runner);
    let connectors = service.list_connectors();

    let mut keys_by_type = HashMap::new();
    let mut allowed: Vec<String> = Vec::new();
    for c in &connectors {
        keys_by_type.insert(c.type_.clone(), c.secret_keys.clone());
        for key in &c.secret_keys {
            if !allowed.contains(key) {
                allowed.push(key.clone());
            }
        }
    }
    allowed.sort();
    Ok((allowed, keys_by_type))
}

/// `GET /api/secrets` — `secrets`: one entry per secret key a
/// *configured* source's connector actually needs (`name`/`set`/
/// `in_env_file`/`in_process_env`/`sources`, matching `loadSecrets`'s
/// (`app.js`) per-row rendering exactly); `allowed`: every secret key
/// any *registered* connector declares, configured or not — the wider
/// list `loadSecrets`' "Set another key" picker draws from. `env_file`
/// is the `.env` path being read/written, shown as-is in the UI.
async fn secrets(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let (allowed, keys_by_type) = allowed_secret_keys(&config)?;

        let mut sources_by_key: HashMap<String, Vec<String>> = HashMap::new();
        let mut names: Vec<&String> = config.sources.keys().collect();
        names.sort();
        for name in names {
            let sc = &config.sources[name];
            for key in keys_by_type.get(&sc.type_).into_iter().flatten() {
                sources_by_key
                    .entry(key.clone())
                    .or_default()
                    .push(name.clone());
            }
        }

        let env_path = config.env_file_path();
        let in_env_file = crate::envfile::read_keys(&env_path);
        let mut needed: Vec<&String> = sources_by_key.keys().collect();
        needed.sort();
        let secrets: Vec<Value> = needed
            .into_iter()
            .map(|key| {
                let is_in_env_file = in_env_file.contains(key);
                let is_in_process_env = std::env::var(key).is_ok();
                json!({
                    "name": key,
                    "set": is_in_env_file || is_in_process_env,
                    "in_env_file": is_in_env_file,
                    "in_process_env": is_in_process_env,
                    "sources": sources_by_key[key],
                })
            })
            .collect();

        Ok(json!({
            "env_file": env_path.to_string_lossy(),
            "secrets": secrets,
            "allowed": allowed,
        }))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

/// Request body for `POST /api/secrets` — `app.js`'s `saveSecret` sends
/// exactly `{name, value}`.
#[derive(Deserialize)]
struct SetSecretRequest {
    name: String,
    value: String,
}

/// `POST /api/secrets` — writes one secret to the `.env` file via
/// [`crate::envfile::set_var`], after checking `name` is a key some
/// registered connector actually declares (rejecting an arbitrary env
/// var name a client might try to inject). `shadowed_by_process_env`
/// mirrors `dbs_core::resolve_passphrase`'s own precedence: a
/// process-env value of the same name wins over `.env` at runtime, so
/// saving here wouldn't actually take effect until that's unset —
/// `saveSecret` (`app.js`) surfaces this as an informational toast, not
/// an error.
async fn set_secret(
    State(state): State<AppState>,
    Json(body): Json<SetSecretRequest>,
) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let name = body.name.clone();
    tokio::task::spawn_blocking(move || -> Result<(), ApiError> {
        let (allowed, _keys_by_type) = allowed_secret_keys(&config)?;
        if !allowed.contains(&body.name) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "{:?} is not a secret key any registered connector declares",
                    body.name
                ),
            ));
        }
        crate::envfile::set_var(&config.env_file_path(), &body.name, &body.value)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    let shadowed_by_process_env = std::env::var(&name).is_ok();
    Ok(Json(
        json!({"name": name, "shadowed_by_process_env": shadowed_by_process_env}),
    ))
}

/// `DELETE /api/secrets/:name` — removes one secret from the `.env`
/// file via [`crate::envfile::unset_var`]. Unlike `set_secret`, `name`
/// isn't checked against the registered-connector allow-list: clearing
/// a stray/no-longer-declared key someone previously saved should still
/// work, matching `unset_var`'s own "missing key is a no-op, not an
/// error" contract.
async fn delete_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let name_for_task = name.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<bool, ApiError> {
        crate::envfile::unset_var(&config.env_file_path(), &name_for_task)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({"name": name, "removed": removed})))
}

// -- backup trigger + progress (issue #174) ------------------------------

/// Request body for `POST /api/backup` — `app.js`'s `startBackup` sends
/// exactly `{source}` (the "Run" button on one source) or `{all: true}`
/// ("Backup all sources").
#[derive(Deserialize, serde::Serialize)]
struct StartBackupRequest {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    all: bool,
}

/// Bridges a [`crate::jobs::Job`]'s cooperative-cancel flag into a real
/// `dbs_core::CancelToken` [`BackupSourceOptions`]/[`BackupAllOptions`]
/// can poll — the two types are structurally identical (`Arc<AtomicBool>`
/// wrappers) but live in different crates with no shared trait, so a
/// small watcher thread is the bridge: it polls `job.is_cancelled()`
/// until either that fires (and sets `core_cancel` to match) or the
/// caller signals the run itself finished via the returned guard's
/// `Drop`.
struct CancelBridge {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CancelBridge {
    fn spawn(job: Arc<Job>, core_cancel: CancelToken) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = done.clone();
        let handle = std::thread::spawn(move || {
            while !done_for_thread.load(Ordering::Relaxed) {
                if job.is_cancelled() {
                    core_cancel.cancel();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        Self {
            done,
            handle: Some(handle),
        }
    }
}

impl Drop for CancelBridge {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A [`ProgressSink`] that forwards every [`ProgressEvent`] straight
/// onto a [`Job`] as-is — its shape already matches what `openProgress`'s
/// (`app.js`) `EventSource.onmessage` reads, field for field, including
/// `SourceDone`'s inline `result` the live progress bar's `doneCount`
/// increments on. Deliberately does *not* also call `Job::record_result`
/// here: a disabled/VPN-skipped/locked/dry-run source's `RunResult`
/// never reaches `on_progress` at all (`backup_source` returns before
/// ever calling `sink.emit` on those early-exit paths) — the job's own
/// `results` list is populated once, after the call returns, from
/// `backup_source`/`backup_all`'s actual return value instead (mirrors
/// `dbs-cli`'s `cmd_backup`, which prints from that same return value,
/// not from progress events), so no skipped source silently goes
/// missing from `snap.results`.
struct JobProgressSink {
    job: Arc<Job>,
}

impl ProgressSink for JobProgressSink {
    fn emit(&self, event: &ProgressEvent) {
        self.job
            .emit(serde_json::to_value(event).unwrap_or(Value::Null));
    }
}

/// `POST /api/backup` — starts a `backup_source`/`backup_all` run as a
/// background [`crate::jobs::Job`] and returns its snapshot immediately
/// (`openProgress` reads `id`/`spec`/`stopping` off it to arm the SSE
/// stream and the Stop button). A `source` that isn't configured is
/// rejected synchronously (a cheap in-memory lookup, no DB open needed);
/// everything else about whether the run itself succeeds surfaces later
/// through the job's own `status`/`error`, same as every other
/// `crate::jobs::Job` consumer.
async fn start_backup(
    State(state): State<AppState>,
    Json(body): Json<StartBackupRequest>,
) -> Result<Json<JobSnapshot>, ApiError> {
    if !body.all {
        match &body.source {
            None => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "either \"source\" or \"all\" must be given".to_string(),
                ))
            }
            Some(name) if !state.config.sources.contains_key(name) => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("no such source: {name:?}"),
                ))
            }
            Some(_) => {}
        }
    }

    let config = state.config.clone();
    let spec = serde_json::to_value(&body).unwrap_or(Value::Null);
    let all = body.all;
    let source = body.source;
    let result = state.job_manager.start(spec, move |job| {
        let sink = JobProgressSink { job: job.clone() };
        let core_cancel = CancelToken::new();
        let bridge = CancelBridge::spawn(job.clone(), core_cancel.clone());

        let mut storage = open_storage(&config).map_err(|e| e.to_string())?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let outcome = if all {
            let opts = BackupAllOptions {
                on_progress: Some(&sink),
                cancel: Some(core_cancel),
                ..Default::default()
            };
            service.backup_all(&opts).map(|results| {
                for result in &results {
                    job.record_result(serde_json::to_value(result).unwrap_or(Value::Null));
                }
            })
        } else {
            let opts = BackupSourceOptions {
                on_progress: Some(&sink),
                cancel: Some(core_cancel),
                ..Default::default()
            };
            service
                .backup_source(source.as_deref().unwrap_or_default(), &opts)
                .map(|result| {
                    job.record_result(serde_json::to_value(&result).unwrap_or(Value::Null));
                })
        };
        drop(bridge);
        outcome.map_err(|e| e.to_string())
    });

    match result {
        Ok(job) => Ok(Json(job.snapshot())),
        Err(JobAlreadyRunning) => Err(ApiError(
            StatusCode::CONFLICT,
            "a backup is already running".to_string(),
        )),
    }
}

/// `GET /api/backup/current` — the in-flight (or most recently
/// finished) job's snapshot, or `null` if none exists yet this process.
/// `resumeIfRunning` (`app.js`) polls this on page load to reattach its
/// progress panel after a refresh.
async fn current_backup(State(state): State<AppState>) -> Json<Value> {
    match state.job_manager.current() {
        Some(job) => Json(serde_json::to_value(job.snapshot()).unwrap_or(Value::Null)),
        None => Json(Value::Null),
    }
}

/// `POST /api/backup/:id/cancel` — requests the named job's graceful
/// early stop (`stopBackup`, `app.js`). 404s for an unknown job id;
/// otherwise always 200, whether or not the job was actually still
/// running to cancel (mirrors `crate::jobs::JobManager::cancel`'s own
/// "no-op past that point" contract).
async fn cancel_backup(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    if state.job_manager.get(id).is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such job".to_string()));
    }
    let cancelled = state.job_manager.cancel(id);
    Ok(Json(json!({"cancelled": cancelled})))
}
