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

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use dbs_core::service::BackupService;
use dbs_core::{
    build_registry, in_named_netns, named_netns_exists, DbsError, SqliteStorage, Storage,
    SubprocessRunner, VpnGuard, CURRENT_API_VERSION,
};

use crate::AppState;

/// Export formats `dbs export --format` accepts (`dbs-cli/src/main.rs`'s
/// own doc comment on that flag is the source of truth this list
/// mirrors — there's no canonical list exported from `dbs-core` today).
const EXPORT_FORMATS: &[&str] = &[
    "json", "ndjson", "csv", "markdown", "archive", "obsidian", "wiki",
];

/// Every `/api` failure becomes one JSON shape: `{"error": "..."}"`
/// with a status code chosen from the underlying [`DbsError`] variant.
/// `Config`/`Load`/`Run` map to 400 (the request or its target source
/// is the problem); `Storage`/`Connector` map to 500 (something on
/// this server's side went wrong, not the caller's).
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
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
        Json(json!({"error": "verify is not yet implemented (tracked in a follow-up issue)"})),
    )
        .into_response()
}
