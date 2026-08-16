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
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Multipart, Path, Query, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use dbs_core::service::{BackupAllOptions, BackupService, BackupSourceOptions, ProgressSink};
use dbs_core::{
    build_registry, get_exporter, in_named_netns, named_netns_exists, CancelToken, DbsError,
    ExportQuery, ItemRow, ProgressEvent, SqliteStorage, Storage, SubprocessRunner, VpnGuard,
    CURRENT_API_VERSION,
};
use dbs_research::models::VideoMeta;
use dbs_research::notebooklm::{resolve_auth_state, UnimplementedClient};
use dbs_research::pipeline::{
    run_pipeline, run_pipeline_for_videos, SynthesisOptions, DEFAULT_QUESTIONS,
};
use dbs_research::report::render_report;
use dbs_research::youtube_search::yt_dlp_available;

use crate::jobs::{Job, JobAlreadyRunning, JobSnapshot, SseItem};
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
pub(crate) fn open_storage(config: &dbs_core::Config) -> Result<SqliteStorage, DbsError> {
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
        .route("/api/connectors/:type/install", post(install_connector))
        .route("/api/connectors/:type/capture", post(capture_connector))
        .route("/api/sources/:name/capture", post(capture_source))
        .route("/api/connectors/:type/import", post(import_connector))
        .route("/api/sources/:name/import", post(import_source))
        .route("/api/export", get(export_download))
        .route("/api/export/profiles", get(export_profiles))
        .route("/api/export-notes", post(export_notes_route))
        .route("/api/research/meta", get(research_meta))
        .route("/api/research/install", post(research_install))
        .route("/api/research/login", post(research_login))
        .route("/api/research", post(start_research))
        .route("/api/research/current", get(current_research))
        .route("/api/research/:id/stream", get(research_stream))
        .route("/api/research/:id/report", get(research_report))
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

/// Bridges `BackupService::verify` — mirrors the reference's
/// `GET /api/verify` (`src/dbs/web/app.py`).
async fn verify(
    State(state): State<AppState>,
    Query(params): Query<SourceParam>,
) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let report = service.verify(params.source.as_deref())?;
        Ok(json!({
            "ok": report.ok,
            "issues": report.issues.iter().map(|x| json!({
                "source": x.source,
                "kind": x.kind,
                "detail": x.detail,
            })).collect::<Vec<_>>(),
        }))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
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
pub(crate) struct CancelBridge {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CancelBridge {
    pub(crate) fn spawn(job: Arc<Job>, core_cancel: CancelToken) -> Self {
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
pub(crate) struct JobProgressSink {
    pub(crate) job: Arc<Job>,
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
                service.notify_results(&results);
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
                    service.notify_results(std::slice::from_ref(&result));
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

// -- in-UI setup & capture (issue #175) -----------------------------------

/// `dbs serve --no-setup` disables every mutating setup/capture/import
/// route server-side, not just their buttons in the UI (`allow_setup`'s
/// own doc-comment in `lib.rs` names this issue as the one that wires
/// the gate up) — a `--no-setup` server refuses to run installers or
/// write capture files even if a client calls these routes directly.
fn require_setup_enabled(state: &AppState) -> Result<(), ApiError> {
    if state.allow_setup {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "setup routes are disabled (dbs serve --no-setup)".to_string(),
        ))
    }
}

fn job_already_running_error() -> ApiError {
    ApiError(StatusCode::CONFLICT, "a job is already running".to_string())
}

/// `POST /api/connectors/:type/install` — starts `dbs-web::setup`'s
/// dependency-install job (`installConnector`, `app.js`) as a background
/// [`crate::jobs::Job`]; progress streams over the shared
/// `GET /api/setup/:id/stream` mount (`crate::jobs::sse_router`, mounted
/// in `lib.rs` on the same [`crate::jobs::JobManager`] `/api/backup`
/// uses — one at-most-one-job-at-a-time primitive for the whole app,
/// same as the reference's single `SetupManager`).
async fn install_connector(
    State(state): State<AppState>,
    Path(type_): Path<String>,
) -> Result<Json<JobSnapshot>, ApiError> {
    require_setup_enabled(&state)?;
    let config = state.config.clone();
    let type_for_job = type_.clone();
    let result = state
        .job_manager
        .start(json!({"kind": "install", "type": type_}), move |job| {
            let (registry, _report) = build_registry(&config);
            let rc = registry
                .get(&type_for_job)
                .ok_or_else(|| format!("no such connector: {type_for_job:?}"))?;
            let commands = crate::setup::install_commands(rc)
                .ok_or_else(|| "no Python interpreter found on PATH to install with".to_string())?;
            crate::setup::run_install_job(&job, &commands)
        });
    match result {
        Ok(job) => Ok(Json(job.snapshot())),
        Err(JobAlreadyRunning) => Err(job_already_running_error()),
    }
}

/// Shared body for `POST /api/connectors/:type/capture` and
/// `POST /api/sources/:name/capture` — `target` is whichever URL
/// segment the caller sent; [`BackupService::resolve_capture_target`]
/// already accepts either a bare connector type or a configured source
/// name (same lookup `dbs capture` uses CLI-side), so both routes share
/// one implementation. Resolves `target` for real before starting the
/// job (mirrors `cmd_capture`'s own resolve-then-report order) so an
/// unknown connector/source gets a specific 400, not a job that starts
/// only to fail with the generic #99 message.
async fn start_capture_job(
    state: &AppState,
    target: String,
) -> Result<Json<JobSnapshot>, ApiError> {
    require_setup_enabled(state)?;
    let config = state.config.clone();
    let target_for_job = target.clone();
    let result =
        state
            .job_manager
            .start(json!({"kind": "capture", "target": target}), move |job| {
                let mut storage = open_storage(&config).map_err(|e| e.to_string())?;
                let (registry, _report) = build_registry(&config);
                let runner = SubprocessRunner::new(&config);
                let service = BackupService::new(&mut storage, &config, &registry, &runner);
                service
                    .resolve_capture_target(&target_for_job)
                    .map_err(|e| e.to_string())?;
                crate::setup::run_capture_job(&job, &target_for_job)
            });
    match result {
        Ok(job) => Ok(Json(job.snapshot())),
        Err(JobAlreadyRunning) => Err(job_already_running_error()),
    }
}

async fn capture_connector(
    State(state): State<AppState>,
    Path(type_): Path<String>,
) -> Result<Json<JobSnapshot>, ApiError> {
    start_capture_job(&state, type_).await
}

async fn capture_source(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<JobSnapshot>, ApiError> {
    start_capture_job(&state, name).await
}

/// Writes `data` to `path`, creating parent directories first — the
/// captures directory (`<base_dir>/captures/`) won't exist on a fresh
/// server until the first import.
fn write_capture_file(path: &FsPath, data: &[u8]) -> Result<(), ApiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not create {}: {e}", parent.display()),
            )
        })?;
    }
    std::fs::write(path, data).map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not write {}: {e}", path.display()),
        )
    })
}

/// Shared body for `POST /api/connectors/:type/import` and
/// `POST /api/sources/:name/import` — `apiUpload`'s (`app.js`)
/// counterpart to live capture for a headless server: `dbs capture` on
/// a machine with a display produces a session artifact, uploaded here
/// as a single `multipart/form-data` `file` field. Validated with the
/// same functions `dbs-web::setup` already built for this
/// (`validate_netscape_cookies`/`validate_storage_state`/
/// `extract_session_zip`), written to `AuthCapture::target_path` (or a
/// default under `<base_dir>/captures/` when the connector doesn't
/// declare one — every real connector today doesn't), and registered
/// as a secret via `dbs-web::envfile` keyed by `AuthCapture::secret_key`
/// — exactly what the issue body specifies.
async fn import_capture(
    state: &AppState,
    target: String,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    require_setup_enabled(state)?;
    let mut data: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?
    {
        if field.name() == Some("file") {
            data = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?
                    .to_vec(),
            );
        }
    }
    let data = data.ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "no \"file\" field in the upload".to_string(),
        )
    })?;

    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let (_rc, spec) = service.resolve_capture_target(&target)?;

        if spec.secret_key.is_empty() {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{target:?}'s connector declares no secret_key for its capture"),
            ));
        }

        let default_path = |suffix: &str| -> PathBuf {
            config
                .base_dir
                .join("captures")
                .join(format!("{target}-{suffix}"))
        };
        let explicit_path =
            (!spec.target_path.is_empty()).then(|| PathBuf::from(&spec.target_path));

        let target_path = match spec.kind.as_str() {
            "browser_session" => {
                let dir = explicit_path.unwrap_or_else(|| default_path("session"));
                crate::setup::extract_session_zip(&data, &dir)
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
                dir
            }
            "browser_cookies" => {
                let text = String::from_utf8_lossy(&data);
                crate::setup::validate_netscape_cookies(&text)
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
                let path = explicit_path.unwrap_or_else(|| default_path("cookies.txt"));
                write_capture_file(&path, &data)?;
                path
            }
            "browser_storage_state" => {
                let text = String::from_utf8_lossy(&data);
                crate::setup::validate_storage_state(&text)
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
                let path = explicit_path.unwrap_or_else(|| default_path("storage_state.json"));
                write_capture_file(&path, &data)?;
                path
            }
            other => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("unsupported capture kind: {other:?}"),
                ))
            }
        };

        crate::envfile::set_var(
            &config.env_file_path(),
            &spec.secret_key,
            &target_path.to_string_lossy(),
        )
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        Ok(json!({
            "note": format!("Imported \u{2014} {} set.", spec.secret_key),
        }))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

async fn import_connector(
    State(state): State<AppState>,
    Path(type_): Path<String>,
    multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    import_capture(&state, type_, multipart).await
}

async fn import_source(
    State(state): State<AppState>,
    Path(name): Path<String>,
    multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    import_capture(&state, name, multipart).await
}

// -- export (issue #176) --------------------------------------------------

/// `GET /api/export/profiles` — each source's resolved export rules
/// and which fields its config overrode. Mirrors the reference's
/// `GET /api/export/profiles` (`src/dbs/web/app.py`).
async fn export_profiles(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = state.config.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let mut profiles: Vec<(String, dbs_core::ExportProfile)> =
            service.export_profiles().into_iter().collect();
        profiles.sort_by(|a, b| a.0.cmp(&b.0));
        let out: Vec<Value> = profiles
            .into_iter()
            .map(|(name, profile)| {
                let type_ = config
                    .sources
                    .get(&name)
                    .map(|sc| sc.type_.clone())
                    .unwrap_or_default();
                let overridden = source_export_overrides(&config, &name);
                json!({
                    "source": name,
                    "type": type_,
                    "enabled": profile.enabled,
                    "item_kinds": profile.item_kinds,
                    "group_by": profile.group_by,
                    "body_from": profile.body_from,
                    "page_per": profile.page_per,
                    "overridden": overridden,
                })
            })
            .collect();
        Ok(json!({"profiles": out}))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

/// Which of a source's `[sources.NAME.export]` fields it actually set —
/// the reference's `overridden` field. Mirrors `dbs-cli`'s identical
/// `source_export_overrides` helper (`main.rs`); duplicated rather
/// than shared since the two crates don't otherwise depend on each
/// other in that direction and it's a small pure function.
fn source_export_overrides(cfg: &dbs_core::Config, name: &str) -> Vec<String> {
    let Some(over) = cfg.sources.get(name).and_then(|sc| sc.export.as_ref()) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    if over.enabled.is_some() {
        fields.push("enabled".to_string());
    }
    if over.item_kinds.is_some() {
        fields.push("item_kinds".to_string());
    }
    if over.group_by.is_some() {
        fields.push("group_by".to_string());
    }
    if over.body_from.is_some() {
        fields.push("body_from".to_string());
    }
    if over.page_per.is_some() {
        fields.push("page_per".to_string());
    }
    fields
}

/// `GET /api/export` — a real file download, not JSON: the `export`
/// form (`app.js`) submits by navigating the browser straight to this
/// URL (`window.location.assign`), so the response has to carry its
/// own `Content-Type`/`Content-Disposition`, matching what a `dbs
/// export --out ...` run on the CLI would produce for the same
/// `format`. `dbs_core::Exporter::media_type`/`file_ext` already exist
/// for exactly this — their own doc-comments call out "the seam a
/// future web layer would use" — so there's no format→extension table
/// to invent here. No `encrypt`/passphrase support: the shipped
/// frontend's export form has no such field (`app.js` never references
/// one), so `BackupService::export`'s `encrypt_passphrase` parameter is
/// simply never used from this route.
async fn export_download(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let q = MultiQuery::parse(raw.as_deref());
    let format = q.one("format").unwrap_or_else(|| "json".to_string());
    let sources = q.all("source");
    let item_types = q.all("type");
    let since = parse_date_param(q.one("since"))?;
    let until = parse_date_param(q.one("until"))?;
    let include_deleted = q.one("include_deleted").as_deref() == Some("true");
    let include_revisions = q.one("include_revisions").as_deref() == Some("true");
    let no_raw = q.one("no_raw").as_deref() == Some("true");
    let wiki_grouping = q
        .one("wiki_grouping")
        .unwrap_or_else(|| "topic".to_string());

    // `get_exporter` just validates `format` and hands back a lookup
    // table entry — extract the two `String`s this handler actually
    // needs and drop the `Box<dyn Exporter>` immediately: it isn't
    // `Send`, and holding it live across the `spawn_blocking` `.await`
    // below would make this handler's future non-`Send`, which axum
    // requires.
    let (media_type, ext) = {
        let exporter = get_exporter(&format)?;
        (
            exporter.media_type().to_string(),
            exporter.file_ext().to_string(),
        )
    };
    let ext_for_job = ext.clone();

    let config = state.config.clone();
    let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, DbsError> {
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
            include_revisions,
            include_raw: !no_raw,
            wiki_grouping,
            ..Default::default()
        };
        // Mirrors `notes_export.rs`'s own `temp_zip_path` convention —
        // a process-id + nanosecond-timestamp temp name, cleaned up
        // right after being read back below.
        let tmp_path = std::env::temp_dir().join(format!(
            "dbs-web-export-{}-{}{ext_for_job}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        ));
        service.export(&query, &format, &tmp_path, None)?;
        let data = std::fs::read(&tmp_path)
            .map_err(|e| DbsError::Storage(format!("failed to read export file: {e}")))?;
        let _ = std::fs::remove_file(&tmp_path);
        Ok(data)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, media_type),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"export{ext}\""),
            ),
        ],
        data,
    )
        .into_response())
}

/// Request body for `POST /api/export-notes` — `app.js`'s notes-export
/// form submit sends exactly `{out_dir, source, type, since, full}`.
#[derive(Deserialize)]
struct ExportNotesRequest {
    out_dir: String,
    #[serde(default)]
    source: Vec<String>,
    #[serde(rename = "type", default)]
    item_type: Vec<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    full: bool,
}

/// `POST /api/export-notes` — bridges `dbs_core::export_notes` (one
/// Markdown note per live item, written directly into `out_dir` rather
/// than a single downloadable file, so this returns JSON summarizing
/// the write instead of streaming a response like `/api/export`).
/// `full` inverted is `incremental`, exactly mirroring `cmd_export_notes`'s
/// (`dbs-cli/src/main.rs`) own `!full` wiring. `out_dir` is used as
/// given, same trust boundary as every other `dbs serve` mutation
/// (loopback-only by default, an optional bearer token otherwise) —
/// the CLI's own `export-notes` command accepts an arbitrary directory
/// from its caller too.
async fn export_notes_route(
    State(state): State<AppState>,
    Json(body): Json<ExportNotesRequest>,
) -> Result<Json<Value>, ApiError> {
    let since = parse_date_param(body.since)?;
    let config = state.config.clone();
    let out_dir = PathBuf::from(&body.out_dir);
    let sources = body.source;
    let item_types = body.item_type;
    let incremental = !body.full;
    let result = tokio::task::spawn_blocking(move || -> Result<Value, DbsError> {
        let mut storage = open_storage(&config)?;
        let (registry, _report) = build_registry(&config);
        let runner = SubprocessRunner::new(&config);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let sources_opt = (!sources.is_empty()).then_some(sources);
        let item_types_opt = (!item_types.is_empty()).then_some(item_types);
        let result = dbs_core::export_notes(
            &service,
            &out_dir,
            sources_opt.as_deref(),
            item_types_opt.as_deref(),
            since,
            incremental,
        )?;
        Ok(json!({
            "item_count": result.item_count,
            "path": result.path,
            "since": result.extra.get("since").and_then(Value::as_str).unwrap_or(""),
        }))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}

// -- research (issue #177) -------------------------------------------------

/// Converts one backed-up YouTube item row (from
/// `BackupService::select_youtube_backup_videos`) into a
/// `dbs-research` [`VideoMeta`] — the two crates deliberately don't
/// depend on each other (`dbs-research`'s own module doc-comment: this
/// pipeline "has nothing to do with the Connector/Storage/engine
/// machinery"), so `dbs-web`, the app layer, is where that conversion
/// belongs. `subscriber_count`/`upload_date` are always `None`: the
/// youtube connector's own handshake never captures them (see its
/// `raw` field construction), only `id`/`title`/`channel`/
/// `view_count`/`duration_seconds` — `VideoMeta::engagement()` already
/// treats a missing subscriber count as "rank last," not an error.
fn item_row_to_video_meta(row: &ItemRow) -> Option<VideoMeta> {
    let raw = row.get("raw").and_then(Value::as_object)?;
    let id = raw.get("id").and_then(Value::as_str)?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let title = row
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    let url = row
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
    Some(VideoMeta {
        id,
        title,
        url,
        channel: raw
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_string),
        subscriber_count: None,
        view_count: raw.get("view_count").and_then(Value::as_i64),
        duration_seconds: raw.get("duration_seconds").and_then(Value::as_i64),
        upload_date: None,
    })
}

/// `GET /api/research/meta` — `loadResearch` (`app.js`) reads:
/// `ready`/`pip_requirements`/`missing` (yt-dlp, the pipeline's one
/// real installable dependency; the NotebookLM synthesis half has no
/// installable adapter at all yet — see `dbs_research::notebooklm`'s
/// module doc-comment — so it's reported separately below, not folded
/// into "missing"), `auth.configured` (whether a captured NotebookLM
/// session exists), `youtube_sources` (for the backup-mode source
/// picker), and `default_questions` (the placeholder text).
async fn research_meta(State(state): State<AppState>) -> Json<Value> {
    let ready = yt_dlp_available();
    let pip_requirements = vec!["yt-dlp".to_string()];
    let missing = if ready {
        Vec::new()
    } else {
        pip_requirements.clone()
    };
    let configured = resolve_auth_state(&state.config.base_dir).is_some();
    let mut youtube_sources: Vec<&String> = state
        .config
        .sources
        .iter()
        .filter(|(_, sc)| sc.type_ == "youtube")
        .map(|(name, _)| name)
        .collect();
    youtube_sources.sort();
    Json(json!({
        "ready": ready,
        "pip_requirements": pip_requirements,
        "missing": missing,
        "auth": {"configured": configured},
        "youtube_sources": youtube_sources,
        "default_questions": DEFAULT_QUESTIONS,
    }))
}

/// `POST /api/research/install` — `pip install yt-dlp`, streamed
/// through the same `/api/setup/:id/stream` mount every other setup
/// job (#175's connector install/capture) uses, on the same shared
/// `job_manager`.
async fn research_install(State(state): State<AppState>) -> Result<Json<JobSnapshot>, ApiError> {
    require_setup_enabled(&state)?;
    let result = state
        .job_manager
        .start(json!({"kind": "research-install"}), |job| {
            let commands = crate::setup::research_install_commands()
                .ok_or_else(|| "no Python interpreter found on PATH to install with".to_string())?;
            crate::setup::run_install_job(&job, &commands)
        });
    match result {
        Ok(job) => Ok(Json(job.snapshot())),
        Err(JobAlreadyRunning) => Err(job_already_running_error()),
    }
}

/// `POST /api/research/login` — NotebookLM's login capture, same
/// shared `job_manager`/`/api/setup/:id/stream` as `research_install`
/// and #175's connector capture; fails cleanly pending a dedicated
/// login-capture script this port hasn't written yet (see
/// `dbs-web::setup`'s module doc-comment).
async fn research_login(State(state): State<AppState>) -> Result<Json<JobSnapshot>, ApiError> {
    require_setup_enabled(&state)?;
    let result = state
        .job_manager
        .start(json!({"kind": "research-login"}), |job| {
            crate::setup::run_notebooklm_login_job(&job)
        });
    match result {
        Ok(job) => Ok(Json(job.snapshot())),
        Err(JobAlreadyRunning) => Err(job_already_running_error()),
    }
}

/// Reshapes a generic [`JobSnapshot`] JSON value into what
/// `app.js`'s research consumers (`openResearchProgress`'s `end`
/// listener, `resumeResearchIfRunning`) actually read: `result`
/// (singular — the one recorded `{report, indexed, total}` object, or
/// `null` before the job finishes) instead of `results` (plural,
/// every other job kind's shape), plus a top-level `connector` hoisted
/// out of `spec` (`openProgress`'s `Researching: ${job.connector}`
/// title reads it directly off the job, not off `spec`).
fn reshape_research_snapshot(mut snapshot: Value) -> Value {
    if let Some(map) = snapshot.as_object_mut() {
        let result = map
            .get("results")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first().cloned());
        map.remove("results");
        map.insert("result".to_string(), result.unwrap_or(Value::Null));
        let connector = map
            .get("spec")
            .and_then(|s| s.get("connector"))
            .cloned()
            .unwrap_or(Value::Null);
        map.insert("connector".to_string(), connector);
    }
    snapshot
}

/// Request body for `POST /api/research` — `app.js`'s research form
/// submit sends exactly this shape (`$("#research-form")`'s submit
/// handler).
#[derive(Deserialize)]
struct StartResearchRequest {
    #[serde(default)]
    mode: String,
    topic: String,
    #[serde(default)]
    queries: Vec<String>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    lists: Vec<String>,
    #[serde(default)]
    questions: Vec<String>,
    #[serde(default = "default_research_count")]
    count: u32,
    #[serde(default = "default_research_per_query_count")]
    per_query_count: u32,
    #[serde(default = "default_research_months")]
    months: u32,
    #[serde(default)]
    infographic: bool,
    #[serde(default)]
    notebook_name: String,
}

fn default_research_count() -> u32 {
    10
}

fn default_research_per_query_count() -> u32 {
    10
}

fn default_research_months() -> u32 {
    6
}

/// `POST /api/research` — starts the YouTube-search-or-backup →
/// NotebookLM-synthesis → report pipeline as a background
/// [`crate::jobs::Job`] on `research_job_manager` (kept apart from the
/// shared setup/backup manager — see `AppState::research_job_manager`'s
/// doc-comment). Every real run fails cleanly at the NotebookLM step
/// (`dbs_research::notebooklm::UnimplementedClient` — Decision 4's
/// real `nlm`/`notebooklm-mcp` adapter is deferred pending that tool's
/// confirmed CLI surface), but search/selection, progress events, and
/// report rendering are all real up to that point.
async fn start_research(
    State(state): State<AppState>,
    Json(body): Json<StartResearchRequest>,
) -> Result<Json<Value>, ApiError> {
    let topic = body.topic.trim().to_string();
    if topic.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "\"topic\" is required".to_string(),
        ));
    }
    let backup_mode = body.mode == "backup";
    let config = state.config.clone();
    let spec = json!({
        "mode": if backup_mode { "backup" } else { "search" },
        "topic": topic,
        "connector": topic,
    });

    let result = state.research_job_manager.start(spec, move |job| {
        let options = SynthesisOptions {
            questions: (!body.questions.is_empty()).then_some(body.questions),
            notebook_name: (!body.notebook_name.trim().is_empty()).then_some(body.notebook_name),
            infographic: body.infographic,
            infographic_orientation: None,
            infographic_path: None,
        };
        let mut client = UnimplementedClient;
        let job_for_progress = job.clone();
        let on_progress = move |line: &str| {
            job_for_progress.emit(json!({"line": line}));
        };

        let pipeline_result = if backup_mode {
            let mut storage = open_storage(&config).map_err(|e| e.to_string())?;
            let (registry, _report) = build_registry(&config);
            let runner = SubprocessRunner::new(&config);
            let service = BackupService::new(&mut storage, &config, &registry, &runner);
            let sources = (!body.sources.is_empty()).then_some(body.sources.as_slice());
            let lists = (!body.lists.is_empty()).then_some(body.lists.as_slice());
            let rows = service
                .select_youtube_backup_videos(sources, lists, Some(body.count as usize))
                .map_err(|e| e.to_string())?;
            let videos: Vec<VideoMeta> = rows.iter().filter_map(item_row_to_video_meta).collect();
            let source_label = if body.sources.is_empty() {
                "backed-up youtube sources".to_string()
            } else {
                body.sources.join(", ")
            };
            run_pipeline_for_videos(
                &topic,
                videos,
                &source_label,
                options,
                &mut client,
                on_progress,
            )
        } else {
            let queries = if body.queries.is_empty() {
                vec![topic.clone()]
            } else {
                body.queries
            };
            run_pipeline(
                &topic,
                &queries,
                body.per_query_count,
                body.count as usize,
                Some(body.months),
                options,
                &mut client,
                on_progress,
            )
        };

        let result = pipeline_result.map_err(|e| e.to_string())?;
        let report = render_report(&result);
        let indexed = result.indexed_videos().len();
        let total = result.outcomes.len();
        job.record_result(json!({"report": report, "indexed": indexed, "total": total}));
        Ok(())
    });

    match result {
        Ok(job) => Ok(Json(reshape_research_snapshot(
            serde_json::to_value(job.snapshot()).unwrap_or(Value::Null),
        ))),
        Err(JobAlreadyRunning) => Err(job_already_running_error()),
    }
}

/// `GET /api/research/current` — the in-flight (or most recently
/// finished) research job's snapshot, or `null`. `resumeResearchIfRunning`
/// (`app.js`) polls this on page load to reattach the progress panel.
async fn current_research(State(state): State<AppState>) -> Json<Value> {
    match state.research_job_manager.current() {
        Some(job) => Json(reshape_research_snapshot(
            serde_json::to_value(job.snapshot()).unwrap_or(Value::Null),
        )),
        None => Json(Value::Null),
    }
}

/// `GET /api/research/:id/stream` — a dedicated SSE handler (not
/// `crate::jobs::sse_router`, whose generic `end` payload shape
/// doesn't match what `openResearchProgress`'s listener reads) built
/// on the same buffered/live/terminal [`Job::subscribe`] primitive
/// every other job stream uses, reshaping only the terminal `end`
/// event's payload via [`reshape_research_snapshot`].
async fn research_stream(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    let Some(job) = state.research_job_manager.get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let events = job.subscribe().map(|item| {
        let event = match item {
            SseItem::Data(v) => Event::default().data(v.to_string()),
            SseItem::End(v) => Event::default()
                .event("end")
                .data(reshape_research_snapshot(v).to_string()),
        };
        Ok::<_, std::convert::Infallible>(event)
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// `GET /api/research/:id/report` — the finished job's rendered
/// Markdown report as a real download (`$("#research-download").href`,
/// `app.js`), read back from the job's own recorded result rather than
/// re-rendering from a re-parsed `ResearchResult` (the report text is
/// all `job.record_result` ever stored — see `start_research`).
async fn research_report(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Response, ApiError> {
    let Some(job) = state.research_job_manager.get(id) else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such job".to_string()));
    };
    let snap = job.snapshot();
    let report = snap
        .results
        .first()
        .and_then(|r| r.get("report"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "no report available for this job".to_string(),
            )
        })?;
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "text/markdown; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"research-report.md\"".to_string(),
            ),
        ],
        report,
    )
        .into_response())
}
