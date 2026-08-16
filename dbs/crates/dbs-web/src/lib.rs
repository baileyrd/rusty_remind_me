//! Web app for `dbs serve` (issue #79), mirroring the shape of the
//! reference's `dbs.web.app.create_app`. The [`jobs`] module (issue
//! #80) adds the background-job manager + SSE primitive long-running
//! `/api` routes build on; the [`auth`] module (issue #81) adds the
//! security gate every route in this router — including the static
//! SPA and `/api` — runs behind; the [`api`] module (issue #170, first
//! slice of #169's umbrella) is the actual `/api` route layer bridging
//! into `dbs-core`. This crate root serves the static single-page app
//! (`GET /` renders `index.html` with its `{{v}}` cache-bust
//! placeholder substituted, mirroring the reference's `index()` route;
//! `GET /static/<name>` serves the SPA's other assets) and assembles
//! the full [`router`].
//!
//! The static assets themselves (`static/index.html`, `static/app.js`,
//! `static/style.css`) are an unmodified copy of the reference's
//! `dbs/web/static/*` — a hand-written vanilla-JS SPA, not a build
//! pipeline output. Its `fetch("/api/...")` calls only resolve against
//! whichever slice of #169's umbrella has landed so far — see
//! `gap-analysis.md`'s Web tier rows for what's real today. Shipping it
//! unmodified from #79 onward meant every later `/api` issue landed
//! against a frontend that was already real instead of needing its own
//! follow-up port.
//!
//! # Sync/async boundary
//!
//! This is the first async code in the workspace — everything else
//! (`dbs-core`, `dbs-cli`'s non-`serve` commands) is deliberately
//! synchronous (see the `reqwest::blocking` decision behind issue #22).
//! The boundary is drawn at the `dbs serve` CLI entry point: `dbs-cli`
//! stays fully synchronous everywhere else, and only `cmd_serve`
//! constructs a dedicated Tokio runtime to drive this crate's async
//! [`serve`]. Every [`api`] handler crosses into `dbs-core`'s
//! synchronous, `&mut dyn Storage`-based API via
//! `tokio::task::spawn_blocking` at the call site, not by growing
//! `dbs-core` an async-facing wrapper — `dbs-core` has no reason to
//! know Tokio exists.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

pub mod api;
pub mod auth;
pub mod envfile;
pub mod jobs;
mod scheduler;
pub mod setup;

use auth::SecurityConfig;

const INDEX_HTML: &str = include_str!("../static/index.html");

struct StaticAsset {
    name: &'static str,
    bytes: &'static [u8],
    content_type: &'static str,
}

const STATIC_ASSETS: &[StaticAsset] = &[
    StaticAsset {
        name: "app.js",
        bytes: include_bytes!("../static/app.js"),
        content_type: "text/javascript; charset=utf-8",
    },
    StaticAsset {
        name: "style.css",
        bytes: include_bytes!("../static/style.css"),
        content_type: "text/css; charset=utf-8",
    },
];

/// Shared across every handler via Axum's `State` extractor — cheap to
/// clone (`Config` is `Arc`-wrapped) since Axum clones it out per
/// request.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Substituted for `index.html`'s `{{v}}` placeholder, cache-busting
    /// `/static` asset links on every process restart. The reference
    /// derives this from the static files' on-disk mtimes; these assets
    /// are compiled into the binary rather than read from disk at
    /// runtime, so there's no mtime to read — the process start time
    /// serves the same purpose (changes exactly when the served assets
    /// could have changed, i.e. on every new build/restart).
    cache_stamp: String,
    /// Loaded once at `dbs serve` startup (issue #170) — every `/api`
    /// handler opens its own fresh `Storage`/`ConnectorRegistry` per
    /// request from this, matching how each `dbs-cli` `cmd_*` function
    /// already treats config: cheap to reuse, no shared mutable state
    /// to guard across concurrent requests.
    pub(crate) config: Arc<dbs_core::Config>,
    /// `dbs serve --no-setup` inverted — reported by `/api/meta` and
    /// (issue #175) gates whether the in-UI setup routes actually work.
    pub(crate) allow_setup: bool,
    /// `dbs serve --schedule` — reported by `/api/meta`. The scheduler
    /// itself isn't wired into this app skeleton yet (`cmd_serve`'s own
    /// stderr note), so this is honest metadata, not a live toggle.
    pub(crate) scheduler_enabled: bool,
    /// The at-most-one-running-job primitive (issue #80) `/api/backup`
    /// (issue #174) starts a `BackupService::backup_source`/`backup_all`
    /// run on. Shared (not per-request) so a second `POST /api/backup`
    /// while one is running is actually refused, and so `/api/backup/current`
    /// can find whatever's in flight after a page reload. Also backs
    /// every in-UI setup job (issue #175's connector install/capture,
    /// and issue #177's research-deps install / NotebookLM login) —
    /// they all stream through the same `/api/setup/:id/stream` mount
    /// `app.js`'s `streamSetup` always uses.
    pub(crate) job_manager: Arc<jobs::JobManager>,
    /// A *separate* job manager (issue #177) for the main research
    /// pipeline run (`POST /api/research`) specifically — kept apart
    /// from `job_manager` because `/api/research/current` and the
    /// `end` event on `/api/research/:id/stream` need a
    /// research-specific snapshot shape (`result`, singular, not
    /// `results`); sharing the generic manager would risk
    /// `/api/research/current` reporting back an unrelated backup or
    /// connector-install job that happened to start more recently.
    pub(crate) research_job_manager: Arc<jobs::JobManager>,
}

fn cache_stamp_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// `dbs serve`'s full runtime options (issue #170) — everything the
/// `/api` layer (see [`api`]) needs beyond `host`/`port`/`token`, which
/// [`router`]/[`serve`] keep as their own separate parameters since
/// they're wire-protocol concerns (bind address, auth), not app state.
pub struct ServeOptions {
    pub config: dbs_core::Config,
    /// `dbs serve --no-setup` inverted.
    pub allow_setup: bool,
    /// `dbs serve --schedule`.
    pub schedule: bool,
}

/// Builds the full app router: `GET /` (the SPA shell), `GET /static/<name>`
/// (its JS/CSS), and the `/api` layer ([`api::router`]) — all behind
/// [`auth::security_gate`]. `token` is `dbs serve --token`; `None`
/// means the DNS-rebinding Host check stays strict (loopback only) and
/// every `/api` request is unauthenticated (fine for the loopback-only
/// default; refused at the `dbs serve` CLI layer otherwise).
pub fn router(token: Option<String>, opts: ServeOptions) -> Router {
    let security_config = Arc::new(SecurityConfig { token });
    let job_manager = Arc::new(jobs::JobManager::new());
    let research_job_manager = Arc::new(jobs::JobManager::new());
    let config = Arc::new(opts.config);
    if opts.schedule {
        scheduler::spawn(
            config.clone(),
            job_manager.clone(),
            scheduler::TICK_INTERVAL,
        );
    }
    let state = AppState {
        cache_stamp: cache_stamp_now(),
        config,
        allow_setup: opts.allow_setup,
        scheduler_enabled: opts.schedule,
        job_manager: job_manager.clone(),
        research_job_manager,
    };
    Router::new()
        .route("/", get(index))
        .route("/static/:name", get(static_asset))
        .merge(api::router())
        .with_state(state)
        .nest("/api/backup", jobs::sse_router(job_manager.clone()))
        .nest("/api/setup", jobs::sse_router(job_manager))
        .layer(axum::middleware::from_fn_with_state(
            security_config,
            auth::security_gate,
        ))
}

async fn index(State(state): State<AppState>) -> Response {
    let html = INDEX_HTML.replace("{{v}}", &state.cache_stamp);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

async fn static_asset(AxumPath(name): AxumPath<String>) -> Response {
    match STATIC_ASSETS.iter().find(|a| a.name == name) {
        Some(asset) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, asset.content_type)],
            asset.bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Binds `host:port` and serves the app skeleton until the process
/// stops (Ctrl+C, or a signal) — mirrors the reference's blocking
/// `uvicorn.run`. Returns only on a bind/accept error.
///
/// `host` is resolved through [`std::net::ToSocketAddrs`], which for a
/// bare hostname defers to the OS resolver — on a host where that
/// resolver prefers IPv6, binding the literal string `"localhost"` can
/// land on `[::1]` instead of `127.0.0.1`, silently refusing IPv4
/// callers (observed as CI-only flakiness that a local run's resolver
/// order didn't reproduce). `""`/`"localhost"` are normalized to the
/// literal `127.0.0.1` here so the bind address is deterministic
/// regardless of resolver configuration; any other host (an explicit
/// IP, or a real non-loopback name) passes through unchanged.
pub async fn serve(
    host: &str,
    port: u16,
    token: Option<String>,
    opts: ServeOptions,
) -> std::io::Result<()> {
    let host = if host.is_empty() || host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    axum::serve(listener, router(token, opts)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn body_string(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// A minimal, valid `Config` for router-level tests — no sources,
    /// an in-memory database, VPN disabled. `Config` has no `Default`
    /// impl (every field is meaningful production config), so this is
    /// the one place in the crate that constructs one by hand.
    fn test_config() -> dbs_core::Config {
        dbs_core::Config {
            database: ":memory:".to_string(),
            export_dir: "exports".to_string(),
            download_root: "downloads".to_string(),
            default_overlap_seconds: 0,
            vpn_exec: String::new(),
            vpn_status: String::new(),
            vpn_netns: String::new(),
            vpn_guard: dbs_core::VpnGuard::default(),
            notify_url: None,
            notify_on: Default::default(),
            http_timeout: 30.0,
            http_rate_limit_per_min: 0,
            batch_max: 500,
            sweep_safety_fraction: 0.5,
            parallel: 1,
            sources: std::collections::HashMap::new(),
            connectors: std::collections::HashMap::new(),
            connectors_dir: None,
            base_dir: std::path::PathBuf::from("."),
            source_path: None,
        }
    }

    fn test_opts() -> ServeOptions {
        ServeOptions {
            config: test_config(),
            allow_setup: true,
            schedule: false,
        }
    }

    /// A real temp-file `Config`+database — `:memory:` doesn't work for
    /// items/media tests (issue #171) since every `/api` handler opens
    /// its own fresh `SqliteStorage` per request, and each `:memory:`
    /// connection is an independent, empty database. Seeds one source
    /// with two items: one plain, one carrying a real (small, opaque)
    /// PNG-shaped image blob as media, exactly the way
    /// `sqlite_storage.rs`'s own `get_media_blob_round_trips_archived_bytes`
    /// test seeds one, so `/api/items`/`/api/items/:id`/`/api/media/:id`/
    /// `/api/thumb/:id` all have something real to find.
    struct SeededDb {
        config: dbs_core::Config,
        plain_item_id: i64,
        image_item_id: i64,
        media_id: i64,
    }

    fn seed_db(label: &str) -> SeededDb {
        use dbs_core::{MediaRef, PreparedItem, SqliteStorage, Storage};

        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-items-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("dbs.sqlite3");

        let mut storage = SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();
        let source = storage
            .upsert_source("a", "raindrop", "p", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();

        let plain = PreparedItem {
            external_id: "e1".to_string(),
            item_kind: "post".to_string(),
            title: Some("Plain item".to_string()),
            url: Some("https://example.com/e1".to_string()),
            body: Some("body one".to_string()),
            tags: vec![],
            item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            item_updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            content_hash: "h1".to_string(),
            raw_json: "{}".to_string(),
            deleted: false,
            media: Vec::new(),
        };
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: Some("x.png".to_string()),
            mime: Some("image/png".to_string()),
            data: Some(b"fake-png-bytes".to_vec()),
        };
        let with_image = PreparedItem {
            external_id: "e2".to_string(),
            item_kind: "post".to_string(),
            title: Some("Item with image".to_string()),
            url: Some("https://example.com/e2".to_string()),
            body: Some("body two".to_string()),
            tags: vec![],
            item_created_at: Some("2026-01-03T00:00:00Z".to_string()),
            item_updated_at: Some("2026-01-04T00:00:00Z".to_string()),
            content_hash: "h2".to_string(),
            raw_json: "{}".to_string(),
            deleted: false,
            media: vec![serde_json::to_value(&media).unwrap()],
        };
        storage
            .upsert_items(source.id, run_id, &[plain, with_image], true, 0)
            .unwrap();

        let (rows, _) = storage
            .browse_items(&dbs_core::ExportQuery::default(), None, 10, 0)
            .unwrap();
        let plain_item_id = rows.iter().find(|r| r["external_id"] == "e1").unwrap()["id"]
            .as_i64()
            .unwrap();
        let image_item_id = rows.iter().find(|r| r["external_id"] == "e2").unwrap()["id"]
            .as_i64()
            .unwrap();
        let media_id: i64 = storage.get_item(image_item_id).unwrap().unwrap()["media"][0]["id"]
            .as_i64()
            .unwrap();

        let mut config = test_config();
        config.database = db_path.to_str().unwrap().to_string();
        SeededDb {
            config,
            plain_item_id,
            image_item_id,
            media_id,
        }
    }

    /// A single `youtube`-typed source with one item whose `url` carries
    /// a real `?v=` video id and no local media — the shape
    /// `thumb`/`thumbUrl` (`app.js`) treat as "derive from YouTube's
    /// CDN instead of a locally stored image".
    fn seed_youtube_item(label: &str) -> (dbs_core::Config, i64) {
        use dbs_core::{PreparedItem, SqliteStorage, Storage};

        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-thumb-youtube-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("dbs.sqlite3");

        let mut storage = SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();
        let source = storage
            .upsert_source("yt", "youtube", "p", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let item = PreparedItem {
            external_id: "v1".to_string(),
            item_kind: "video".to_string(),
            title: Some("A video".to_string()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string()),
            body: None,
            tags: vec![],
            item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            item_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            content_hash: "h1".to_string(),
            raw_json: "{}".to_string(),
            deleted: false,
            media: Vec::new(),
        };
        storage
            .upsert_items(source.id, run_id, &[item], true, 0)
            .unwrap();
        let (rows, _) = storage
            .browse_items(&dbs_core::ExportQuery::default(), None, 10, 0)
            .unwrap();
        let item_id = rows[0]["id"].as_i64().unwrap();

        let mut config = test_config();
        config.database = db_path.to_str().unwrap().to_string();
        (config, item_id)
    }

    #[tokio::test]
    async fn api_items_lists_seeded_items() {
        let seeded = seed_db("list");
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/items").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 2);
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_items_filters_by_search_query() {
        let seeded = seed_db("search");
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/items?q=body+two").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["external_id"], "e2");
    }

    #[tokio::test]
    async fn api_item_detail_returns_the_full_row_with_media() {
        let seeded = seed_db("detail");
        let image_item_id = seeded.image_item_id;
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) =
            get_json(router(None, opts), &format!("/api/items/{image_item_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["external_id"], "e2");
        let media = body["media"].as_array().unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["mime"], "image/png");
        assert_eq!(media[0]["has_data"], true);
    }

    #[tokio::test]
    async fn api_item_detail_404s_for_an_unknown_id() {
        let (status, body) = get_json(router(None, test_opts()), "/api/items/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["detail"].as_str().unwrap().contains("no such item"));
    }

    #[tokio::test]
    async fn api_media_serves_the_raw_bytes_with_the_stored_content_type() {
        let seeded = seed_db("media");
        let media_id = seeded.media_id;
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let response = router(None, opts)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/media/{media_id}"))
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(content_type, "image/png");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"fake-png-bytes");
    }

    #[tokio::test]
    async fn api_media_404s_for_an_unknown_id() {
        let (status, _) = get_json(router(None, test_opts()), "/api/media/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_thumb_serves_an_items_own_local_image_media() {
        let seeded = seed_db("thumb-local");
        let image_item_id = seeded.image_item_id;
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let response = router(None, opts)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/thumb/{image_item_id}"))
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"fake-png-bytes");
    }

    #[tokio::test]
    async fn api_thumb_redirects_to_the_youtube_cdn_for_a_derivable_video() {
        let (config, item_id) = seed_youtube_item("redirect");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let response = router(None, opts)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/thumb/{item_id}"))
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            location,
            "https://img.youtube.com/vi/dQw4w9WgXcQ/mqdefault.jpg"
        );
    }

    #[tokio::test]
    async fn api_thumb_404s_for_a_plain_non_derivable_item() {
        let seeded = seed_db("thumb-none");
        let plain_item_id = seeded.plain_item_id;
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let (status, _) =
            get_json(router(None, opts), &format!("/api/thumb/{plain_item_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn index_serves_the_spa_shell_with_the_cache_bust_placeholder_substituted() {
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(content_type.starts_with("text/html"), "{content_type}");
        let body = body_string(response).await;
        assert!(body.contains("<!DOCTYPE html"), "{body}");
        assert!(!body.contains("{{v}}"), "{body}");
        assert!(body.contains("/static/style.css?v="), "{body}");
        assert!(body.contains("/static/app.js?v="), "{body}");
    }

    #[tokio::test]
    async fn static_serves_app_js_with_a_javascript_content_type() {
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .uri("/static/app.js")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            content_type.starts_with("text/javascript"),
            "{content_type}"
        );
        let body = body_string(response).await;
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn static_serves_style_css_with_a_css_content_type() {
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .uri("/static/style.css")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(content_type.starts_with("text/css"), "{content_type}");
    }

    #[tokio::test]
    async fn static_asset_that_does_not_exist_is_a_404() {
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .uri("/static/nope.js")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_route_is_a_404() {
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_binds_an_ephemeral_localhost_port_and_answers_a_real_request() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(None, test_opts()))
                .await
                .unwrap();
        });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("<!DOCTYPE html"), "{response}");

        server.abort();
    }

    #[tokio::test]
    async fn the_security_gate_is_actually_wired_into_the_router() {
        // Full coverage of the gate's own logic lives in `auth`'s tests;
        // this just confirms `router()` really applies it rather than
        // leaving the middleware unattached.
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_string(response).await.contains("DNS-rebinding"));
    }

    // -- /api (issue #170) ---------------------------------------------

    async fn get_json(router: Router, path: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_string(response).await;
        (
            status,
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn api_meta_reports_tool_and_core_api_versions() {
        let (status, body) = get_json(router(None, test_opts()), "/api/meta").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tool_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["core_api_version"], dbs_core::CURRENT_API_VERSION);
        assert_eq!(body["setup_enabled"], true);
        assert_eq!(body["scheduler_enabled"], false);
        assert!(body["formats"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("ndjson")));
    }

    #[tokio::test]
    async fn api_status_with_no_sources_is_an_empty_array() {
        let (status, body) = get_json(router(None, test_opts()), "/api/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn api_history_with_no_sources_is_an_empty_array() {
        let (status, body) = get_json(router(None, test_opts()), "/api/history").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn api_metrics_on_an_empty_database_is_all_zeroes() {
        let (status, body) = get_json(router(None, test_opts()), "/api/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["by_source_kind"], serde_json::json!([]));
        assert_eq!(body["media_count"], 0);
        assert_eq!(body["media_bytes"], 0);
    }

    #[tokio::test]
    async fn api_vpn_is_not_relevant_when_no_source_requires_it() {
        let (status, body) = get_json(router(None, test_opts()), "/api/vpn").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({"relevant": false, "up": null, "detail": ""})
        );
    }

    #[tokio::test]
    async fn api_vpn_is_down_when_a_source_requires_it_and_the_netns_is_absent() {
        let mut config = test_config();
        config.vpn_netns = "dbs-web-test-netns-that-does-not-exist".to_string();
        config.sources.insert(
            "vpn-source".to_string(),
            dbs_core::SourceConfig {
                name: "vpn-source".to_string(),
                type_: "fake".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: true,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/vpn").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["relevant"], true);
        assert_eq!(body["up"], false);
        assert!(body["detail"].as_str().unwrap().contains("not up"));
    }

    #[tokio::test]
    async fn api_verify_on_an_empty_database_reports_ok() {
        let (status, body) = get_json(router(None, test_opts()), "/api/verify").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["issues"], serde_json::json!([]));
    }

    // -- /api/connectors, /api/sources (issue #172) ---------------------

    /// A real, spawnable `dbs-connector-fixture` shell script that writes
    /// a minimal valid handshake line and exits — same technique
    /// `dbs-core`'s own `registry.rs` tests use to test discovery against
    /// a real subprocess rather than `ConnectorRegistry::from_resolved`'s
    /// bypass, which `dbs-web`'s handlers can't reach (they only ever
    /// build a registry through `dbs_core::build_registry`, i.e. a real
    /// directory scan).
    #[cfg(unix)]
    fn connectors_dir_with_a_fixture_connector(label: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-connectors-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("dbs-connector-fixture");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho '{{\"type\":\"fixture\",\"core_api_version\":{},\"schema_version\":1,\"capabilities\":{{\"requires_auth\":false}},\"item_kinds\":[\"item\"]}}'\nexit 0\n",
                dbs_core::CURRENT_API_VERSION
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[tokio::test]
    async fn api_connectors_with_no_connectors_dir_is_an_empty_array() {
        let (status, body) = get_json(router(None, test_opts()), "/api/connectors").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_connectors_lists_a_discovered_connector_with_its_auth_capture() {
        let dir = connectors_dir_with_a_fixture_connector("list");
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/connectors").await;
        assert_eq!(status, StatusCode::OK);
        let connectors = body.as_array().unwrap();
        assert_eq!(connectors.len(), 1);
        assert_eq!(connectors[0]["type"], "fixture");
        assert_eq!(connectors[0]["auth_capture"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn api_sources_with_none_configured_is_an_empty_array() {
        let (status, body) = get_json(router(None, test_opts()), "/api/sources").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn api_sources_lists_a_configured_source_not_yet_backed_up() {
        let mut config = test_config();
        config.sources.insert(
            "a".to_string(),
            dbs_core::SourceConfig {
                name: "a".to_string(),
                type_: "raindrop".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/sources").await;
        assert_eq!(status, StatusCode::OK);
        let sources = body.as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["name"], "a");
        assert_eq!(sources[0]["type"], "raindrop");
        assert_eq!(sources[0]["enabled"], true);
        assert_eq!(sources[0]["backed_up"], false);
    }

    async fn post_json(
        router: Router,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::HOST, "127.0.0.1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let resp_body = body_string(response).await;
        (
            status,
            serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn api_create_source_rejects_an_unregistered_connector_type() {
        let (status, body) = post_json(
            router(None, test_opts()),
            "/api/sources",
            serde_json::json!({
                "name": "new-source",
                "type": "nonexistent",
                "options": {},
                "store_media": false,
                "max_media_mb": 0,
                "requires_vpn": false,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("nonexistent"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_create_source_adds_a_source_for_a_registered_connector_type() {
        let dir = connectors_dir_with_a_fixture_connector("create");
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        let config_dir = std::env::temp_dir().join(format!(
            "dbs-web-api-create-source-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("dbs.toml");
        std::fs::write(&config_path, "[dbs]\n").unwrap();
        config.source_path = Some(config_path.clone());
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = post_json(
            router(None, opts),
            "/api/sources",
            serde_json::json!({
                "name": "new-source",
                "type": "fixture",
                "options": {},
                "store_media": true,
                "max_media_mb": 50,
                "requires_vpn": false,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "new-source");
        assert_eq!(body["type"], "fixture");
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(written.contains("[sources.new-source]"));
        assert!(written.contains("type = \"fixture\""));
    }

    // -- /api/secrets (issue #173) ---------------------------------------

    /// A temp directory to use as `Config::base_dir` — `test_config()`'s
    /// default (`"."`) would otherwise point `Config::env_file_path` at
    /// a real `./.env` next to wherever `cargo test` happens to run,
    /// which a secrets test must never read from or write to.
    fn temp_base_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-secrets-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Like `connectors_dir_with_a_fixture_connector`, but the fixture
    /// declares `requires_auth: true` and one `secret_key` — the shape
    /// `/api/secrets` needs something to actually list.
    #[cfg(unix)]
    fn connectors_dir_with_a_fixture_connector_requiring_auth(
        label: &str,
        secret_key: &str,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-secrets-connectors-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("dbs-connector-fixture");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho '{{\"type\":\"fixture\",\"core_api_version\":{},\"schema_version\":1,\"capabilities\":{{\"requires_auth\":true}},\"item_kinds\":[\"item\"],\"secret_keys\":[\"{secret_key}\"]}}'\nexit 0\n",
                dbs_core::CURRENT_API_VERSION
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_secrets_with_no_configured_sources_reports_no_secrets_but_lists_allowed() {
        let connectors_dir = connectors_dir_with_a_fixture_connector_requiring_auth(
            "none-configured",
            "FIXTURE_TOKEN",
        );
        let mut config = test_config();
        config.connectors_dir = Some(connectors_dir);
        config.base_dir = temp_base_dir("none-configured");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/secrets").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["secrets"], serde_json::json!([]));
        assert_eq!(body["allowed"], serde_json::json!(["FIXTURE_TOKEN"]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_secrets_lists_a_configured_sources_required_key_as_not_set() {
        let connectors_dir =
            connectors_dir_with_a_fixture_connector_requiring_auth("configured", "FIXTURE_TOKEN");
        let mut config = test_config();
        config.connectors_dir = Some(connectors_dir);
        config.base_dir = temp_base_dir("configured");
        config.sources.insert(
            "a".to_string(),
            dbs_core::SourceConfig {
                name: "a".to_string(),
                type_: "fixture".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/secrets").await;
        assert_eq!(status, StatusCode::OK);
        let secrets = body["secrets"].as_array().unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0]["name"], "FIXTURE_TOKEN");
        assert_eq!(secrets[0]["set"], false);
        assert_eq!(secrets[0]["in_env_file"], false);
        assert_eq!(secrets[0]["in_process_env"], false);
        assert_eq!(secrets[0]["sources"], serde_json::json!(["a"]));
    }

    #[tokio::test]
    async fn api_set_secret_rejects_an_unrecognized_key_name() {
        let mut config = test_config();
        config.base_dir = temp_base_dir("reject-unknown");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = post_json(
            router(None, opts),
            "/api/secrets",
            serde_json::json!({"name": "NOPE", "value": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("NOPE"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_set_secret_writes_the_env_file_and_a_later_get_reflects_it() {
        let connectors_dir =
            connectors_dir_with_a_fixture_connector_requiring_auth("set", "FIXTURE_TOKEN");
        let mut config = test_config();
        config.connectors_dir = Some(connectors_dir);
        config.base_dir = temp_base_dir("set");
        config.sources.insert(
            "a".to_string(),
            dbs_core::SourceConfig {
                name: "a".to_string(),
                type_: "fixture".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        let env_path = config.env_file_path();
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);

        let (status, body) = post_json(
            router.clone(),
            "/api/secrets",
            serde_json::json!({"name": "FIXTURE_TOKEN", "value": "abc123"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "FIXTURE_TOKEN");
        assert_eq!(body["shadowed_by_process_env"], false);
        let written = std::fs::read_to_string(&env_path).unwrap();
        assert!(written.contains("FIXTURE_TOKEN=\"abc123\""));

        let (status, body) = get_json(router, "/api/secrets").await;
        assert_eq!(status, StatusCode::OK);
        let secrets = body["secrets"].as_array().unwrap();
        assert_eq!(secrets[0]["set"], true);
        assert_eq!(secrets[0]["in_env_file"], true);
    }

    #[tokio::test]
    async fn api_delete_secret_removes_it_from_the_env_file() {
        let mut config = test_config();
        config.base_dir = temp_base_dir("delete");
        let env_path = config.env_file_path();
        envfile::set_var(&env_path, "SOME_KEY", "value").unwrap();
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let response = router(None, opts)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/secrets/SOME_KEY")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["removed"], true);
        let written = std::fs::read_to_string(&env_path).unwrap();
        assert!(!written.contains("SOME_KEY"));
    }

    #[tokio::test]
    async fn api_delete_secret_on_a_key_never_set_reports_removed_false() {
        let mut config = test_config();
        config.base_dir = temp_base_dir("delete-absent");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let response = router(None, opts)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/secrets/NEVER_SET")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["removed"], false);
    }

    // -- /api/backup (issue #174) -----------------------------------------

    fn config_with_disabled_source(label: &str) -> dbs_core::Config {
        let mut config = test_config();
        config.base_dir = temp_base_dir(label);
        config.sources.insert(
            "a".to_string(),
            dbs_core::SourceConfig {
                name: "a".to_string(),
                type_: "fixture".to_string(),
                enabled: false,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        config
    }

    /// Polls `GET /api/backup/:id` (the plain JSON snapshot route
    /// `jobs::sse_router` nests under `/api/backup`) until the job is no
    /// longer `running` — every job this test suite starts does real
    /// but near-instant work (a disabled source, or an empty `--all`),
    /// so a short bounded poll is enough without a fake sleep.
    async fn wait_until_done(router: Router, id: u64) -> serde_json::Value {
        for _ in 0..200 {
            let (status, body) = get_json(router.clone(), &format!("/api/backup/{id}")).await;
            assert_eq!(status, StatusCode::OK);
            if body["status"] != "running" {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job {id} never finished");
    }

    #[tokio::test]
    async fn api_start_backup_requires_source_or_all() {
        let (status, body) = post_json(
            router(None, test_opts()),
            "/api/backup",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"]
            .as_str()
            .unwrap()
            .contains("\"source\" or \"all\""));
    }

    #[tokio::test]
    async fn api_start_backup_rejects_an_unconfigured_source() {
        let (status, body) = post_json(
            router(None, test_opts()),
            "/api/backup",
            serde_json::json!({"source": "nonexistent"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("nonexistent"));
    }

    #[tokio::test]
    async fn api_start_backup_runs_a_disabled_source_to_a_skipped_result() {
        let config = config_with_disabled_source("disabled");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (status, body) = post_json(
            router.clone(),
            "/api/backup",
            serde_json::json!({"source": "a"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spec"]["source"], "a");
        let id = body["id"].as_u64().unwrap();

        let snap = wait_until_done(router, id).await;
        assert_eq!(snap["status"], "done");
        let results = snap["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["status"], "skipped");
    }

    #[tokio::test]
    async fn api_backup_current_is_null_when_nothing_has_run() {
        let (status, body) = get_json(router(None, test_opts()), "/api/backup/current").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn api_backup_current_reflects_the_most_recently_started_job() {
        let config = config_with_disabled_source("current");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (_, body) = post_json(
            router.clone(),
            "/api/backup",
            serde_json::json!({"source": "a"}),
        )
        .await;
        let id = body["id"].as_u64().unwrap();
        wait_until_done(router.clone(), id).await;

        let (status, current) = get_json(router, "/api/backup/current").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(current["id"], id);
    }

    #[tokio::test]
    async fn api_cancel_backup_404s_for_an_unknown_job() {
        let response = router(None, test_opts())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/backup/999999/cancel")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_cancel_backup_on_an_already_finished_job_reports_not_cancelled() {
        let config = config_with_disabled_source("cancel-finished");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (_, body) = post_json(
            router.clone(),
            "/api/backup",
            serde_json::json!({"source": "a"}),
        )
        .await;
        let id = body["id"].as_u64().unwrap();
        wait_until_done(router.clone(), id).await;

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/backup/{id}/cancel"))
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["cancelled"], false);
    }

    #[tokio::test]
    async fn api_backup_stream_ends_with_the_final_snapshot() {
        let config = config_with_disabled_source("stream");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (_, body) = post_json(
            router.clone(),
            "/api/backup",
            serde_json::json!({"source": "a"}),
        )
        .await;
        let id = body["id"].as_u64().unwrap();
        wait_until_done(router.clone(), id).await;

        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/api/backup/{id}/stream"))
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sse_body = body_string(response).await;
        assert!(sse_body.contains("event: end"), "{sse_body}");
        assert!(sse_body.contains(r#""status":"done""#), "{sse_body}");
    }

    // -- /api/connectors/:type/install|capture, /api/sources/:name/capture,
    // /api/connectors/:type/import, /api/sources/:name/import (issue #175) --

    /// Like `connectors_dir_with_a_fixture_connector_requiring_auth`
    /// (#173), but the handshake also declares `auth_capture` — the
    /// piece capture/import routes need to resolve a target at all.
    #[cfg(unix)]
    fn connectors_dir_with_a_fixture_auth_capture_connector(
        label: &str,
        kind: &str,
        secret_key: &str,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-setup-connectors-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("dbs-connector-fixture");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho '{{\"type\":\"fixture\",\"core_api_version\":{},\"schema_version\":1,\
                 \"capabilities\":{{\"requires_auth\":true}},\"item_kinds\":[\"item\"],\
                 \"secret_keys\":[\"{secret_key}\"],\"auth_capture\":{{\"kind\":\"{kind}\",\
                 \"secret_key\":\"{secret_key}\",\"login_url\":\"\",\"label\":\"Fixture login\",\
                 \"target_dir_option\":\"\",\"target_path\":\"\",\"per_source\":false}}}}'\nexit 0\n",
                dbs_core::CURRENT_API_VERSION
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// Polls `GET /api/setup/:id` (the plain JSON snapshot route
    /// `jobs::sse_router` nests under `/api/setup`) until the job is no
    /// longer `running`.
    async fn wait_until_setup_job_done(router: Router, id: u64) -> serde_json::Value {
        for _ in 0..200 {
            let (status, body) = get_json(router.clone(), &format!("/api/setup/{id}")).await;
            assert_eq!(status, StatusCode::OK);
            if body["status"] != "running" {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job {id} never finished");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_install_connector_completes_when_nothing_needs_installing() {
        let dir = connectors_dir_with_a_fixture_connector("install-noop");
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (status, body) = post_json(
            router.clone(),
            "/api/connectors/fixture/install",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_setup_job_done(router, id).await;
        assert_eq!(snap["status"], "done");
    }

    #[tokio::test]
    async fn api_install_connector_job_errors_for_an_unregistered_connector_type() {
        let router = router(None, test_opts());
        let (status, body) = post_json(
            router.clone(),
            "/api/connectors/nonexistent/install",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_setup_job_done(router, id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"].as_str().unwrap().contains("nonexistent"));
    }

    #[tokio::test]
    async fn api_install_connector_is_forbidden_when_setup_is_disabled() {
        let opts = ServeOptions {
            config: test_config(),
            allow_setup: false,
            schedule: false,
        };
        let (status, body) = post_json(
            router(None, opts),
            "/api/connectors/fixture/install",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["detail"].as_str().unwrap().contains("--no-setup"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_capture_connector_fails_cleanly_pending_issue_99() {
        let dir = connectors_dir_with_a_fixture_auth_capture_connector(
            "capture-connector",
            "browser_cookies",
            "FIXTURE_COOKIES_FILE",
        );
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (status, body) = post_json(
            router.clone(),
            "/api/connectors/fixture/capture",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_setup_job_done(router, id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"]
            .as_str()
            .unwrap()
            .contains("dedicated Playwright script"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_capture_source_resolves_the_connector_via_the_configured_source_name() {
        let dir = connectors_dir_with_a_fixture_auth_capture_connector(
            "capture-source",
            "browser_session",
            "FIXTURE_SESSION_DIR",
        );
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        config.sources.insert(
            "a".to_string(),
            dbs_core::SourceConfig {
                name: "a".to_string(),
                type_: "fixture".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (status, body) = post_json(
            router.clone(),
            "/api/sources/a/capture",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_setup_job_done(router, id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"]
            .as_str()
            .unwrap()
            .contains("dedicated Playwright script"));
    }

    #[tokio::test]
    async fn api_capture_connector_job_errors_for_an_unresolvable_target() {
        let router = router(None, test_opts());
        let (status, body) = post_json(
            router.clone(),
            "/api/connectors/nonexistent/capture",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_setup_job_done(router, id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"]
            .as_str()
            .unwrap()
            .contains("no such connector or source"));
    }

    /// A hand-built `multipart/form-data` body with one `file` field —
    /// the exact shape `apiUpload` (`app.js`) sends, and simple enough
    /// not to need a multipart-building dependency just for tests.
    fn multipart_file_body(filename: &str, content: &[u8]) -> (String, Vec<u8>) {
        let boundary = "dbsWebTestBoundary".to_string();
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (boundary, body)
    }

    async fn post_multipart_file(
        router: Router,
        path: &str,
        filename: &str,
        content: &[u8],
    ) -> (StatusCode, serde_json::Value) {
        let (boundary, body) = multipart_file_body(filename, content);
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::HOST, "127.0.0.1")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let resp_body = body_string(response).await;
        (
            status,
            serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null),
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_import_connector_writes_cookies_and_registers_the_secret() {
        let dir = connectors_dir_with_a_fixture_auth_capture_connector(
            "import-cookies",
            "browser_cookies",
            "FIXTURE_COOKIES_FILE",
        );
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        config.base_dir = temp_base_dir("import-cookies");
        let env_path = config.env_file_path();
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let cookies = b"# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tFALSE\t0\tsid\tabc\n";
        let (status, body) = post_multipart_file(
            router,
            "/api/connectors/fixture/import",
            "cookies.txt",
            cookies,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["note"]
            .as_str()
            .unwrap()
            .contains("FIXTURE_COOKIES_FILE"));

        let env_contents = std::fs::read_to_string(&env_path).unwrap();
        assert!(env_contents.contains("FIXTURE_COOKIES_FILE="));
        assert!(env_contents.contains("fixture-cookies.txt"));

        let written_path = env_contents
            .lines()
            .find(|l| l.starts_with("FIXTURE_COOKIES_FILE="))
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .trim_matches('"');
        assert_eq!(std::fs::read(written_path).unwrap(), cookies);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_import_connector_rejects_invalid_cookies_content() {
        let dir = connectors_dir_with_a_fixture_auth_capture_connector(
            "import-invalid",
            "browser_cookies",
            "FIXTURE_COOKIES_FILE",
        );
        let mut config = test_config();
        config.connectors_dir = Some(dir);
        config.base_dir = temp_base_dir("import-invalid");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = post_multipart_file(
            router(None, opts),
            "/api/connectors/fixture/import",
            "cookies.txt",
            b"not cookies at all",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("Netscape"));
    }

    #[tokio::test]
    async fn api_import_is_forbidden_when_setup_is_disabled() {
        let opts = ServeOptions {
            config: test_config(),
            allow_setup: false,
            schedule: false,
        };
        let (status, body) = post_multipart_file(
            router(None, opts),
            "/api/connectors/fixture/import",
            "cookies.txt",
            b"whatever",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["detail"].as_str().unwrap().contains("--no-setup"));
    }

    // -- /api/export, /api/export-notes (issue #176) ---------------------

    #[tokio::test]
    async fn api_export_downloads_a_json_file_with_the_right_content_type() {
        let seeded = seed_db("export-json");
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let response = router(None, opts)
            .oneshot(
                Request::builder()
                    .uri("/api/export?format=json")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(content_type, "application/json");
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.contains("export.json"), "{disposition}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_export_rejects_an_unknown_format() {
        let (status, body) = get_json(router(None, test_opts()), "/api/export?format=bogus").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("bogus"));
    }

    #[tokio::test]
    async fn api_export_wiki_grouping_defaults_to_topic_and_accepts_item() {
        // format=wiki's response is a real zip file, not JSON -- check
        // status only, the same way api_export_downloads_a_json_file_
        // with_the_right_content_type does for format=json.
        for uri in [
            "/api/export?format=wiki",
            "/api/export?format=wiki&wiki_grouping=item",
        ] {
            let response = router(None, test_opts())
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::HOST, "127.0.0.1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn api_export_rejects_an_unknown_wiki_grouping() {
        let (status, body) = get_json(
            router(None, test_opts()),
            "/api/export?format=wiki&wiki_grouping=bogus",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("bogus"), "{body}");
    }

    #[tokio::test]
    async fn api_export_profiles_with_no_sources_is_an_empty_array() {
        let (status, body) = get_json(router(None, test_opts()), "/api/export/profiles").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["profiles"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn api_export_profiles_reports_a_sources_resolved_profile_and_overrides() {
        let mut config = test_config();
        config.sources.insert(
            "a".to_string(),
            dbs_core::SourceConfig {
                name: "a".to_string(),
                type_: "raindrop".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: Some(dbs_core::export_profile::ExportProfileOverride {
                    enabled: Some(false),
                    item_kinds: None,
                    group_by: None,
                    body_from: None,
                    page_per: None,
                }),
                options: std::collections::HashMap::new(),
            },
        );
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/export/profiles").await;
        assert_eq!(status, StatusCode::OK);
        let profiles = body["profiles"].as_array().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["source"], "a");
        assert_eq!(profiles[0]["type"], "raindrop");
        assert_eq!(profiles[0]["enabled"], false);
        assert_eq!(profiles[0]["overridden"], serde_json::json!(["enabled"]));
    }

    #[tokio::test]
    async fn api_export_notes_writes_markdown_files_and_reports_the_count() {
        let seeded = seed_db("export-notes");
        let out_dir = std::env::temp_dir().join(format!(
            "dbs-web-api-export-notes-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let opts = ServeOptions {
            config: seeded.config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = post_json(
            router(None, opts),
            "/api/export-notes",
            serde_json::json!({
                "out_dir": out_dir.to_str().unwrap(),
                "source": [],
                "type": [],
                "since": null,
                "full": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["item_count"], 2);
        assert_eq!(body["path"], out_dir.to_str().unwrap());
        let entries: Vec<_> = std::fs::read_dir(&out_dir).unwrap().collect();
        assert!(
            entries.iter().any(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "md")),
            "{entries:?}"
        );
        std::fs::remove_dir_all(&out_dir).ok();
    }

    // -- /api/research (issue #177) ---------------------------------------

    /// A configured `youtube`-type source with one backed-up video
    /// whose `raw` carries a real video id/channel/view/duration —
    /// unlike `seed_youtube_item` (whose `raw_json` is `"{}"`, fine for
    /// the thumbnail-redirect tests but missing the `id` field
    /// `BackupService::select_youtube_backup_videos` requires to
    /// select anything at all).
    fn seed_research_youtube_source(label: &str) -> dbs_core::Config {
        use dbs_core::{PreparedItem, SqliteStorage, Storage};

        let dir = std::env::temp_dir().join(format!(
            "dbs-web-api-research-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("dbs.sqlite3");

        let mut storage = SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();
        let source = storage
            .upsert_source("yt", "youtube", "p", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let item = PreparedItem {
            external_id: "v1".to_string(),
            item_kind: "video".to_string(),
            title: Some("A great video".to_string()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string()),
            body: None,
            tags: vec![],
            item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            item_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            content_hash: "h1".to_string(),
            raw_json: serde_json::json!({
                "id": "dQw4w9WgXcQ",
                "channel": "A Channel",
                "view_count": 12345,
                "duration_seconds": 212,
            })
            .to_string(),
            deleted: false,
            media: Vec::new(),
        };
        storage
            .upsert_items(source.id, run_id, &[item], true, 0)
            .unwrap();

        let mut config = test_config();
        config.database = db_path.to_str().unwrap().to_string();
        config.sources.insert(
            "yt".to_string(),
            dbs_core::SourceConfig {
                name: "yt".to_string(),
                type_: "youtube".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        config
    }

    async fn wait_until_research_job_done(router: Router, id: u64) -> serde_json::Value {
        for _ in 0..200 {
            let (status, body) = get_json(router.clone(), "/api/research/current").await;
            assert_eq!(status, StatusCode::OK);
            if body["id"] == id && body["status"] != "running" {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("research job {id} never finished");
    }

    #[tokio::test]
    async fn api_research_meta_reports_a_well_formed_response() {
        let (status, body) = get_json(router(None, test_opts()), "/api/research/meta").await;
        assert_eq!(status, StatusCode::OK);
        let pip_requirements = body["pip_requirements"].as_array().unwrap();
        assert!(pip_requirements.contains(&serde_json::json!("yt-dlp")));
        let ready = body["ready"].as_bool().unwrap();
        let missing = body["missing"].as_array().unwrap();
        assert_eq!(missing.is_empty(), ready);
        assert_eq!(body["auth"]["configured"], false);
        assert_eq!(body["youtube_sources"], serde_json::json!([]));
        assert_eq!(body["default_questions"].as_array().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn api_research_meta_lists_only_youtube_type_sources() {
        let mut config = test_config();
        config.sources.insert(
            "yt".to_string(),
            dbs_core::SourceConfig {
                name: "yt".to_string(),
                type_: "youtube".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        config.sources.insert(
            "rd".to_string(),
            dbs_core::SourceConfig {
                name: "rd".to_string(),
                type_: "raindrop".to_string(),
                enabled: true,
                schedule: None,
                reconcile_every_runs: None,
                store_media: false,
                max_media_mb: 0,
                requires_vpn: false,
                keep_revisions: 0,
                export: None,
                options: std::collections::HashMap::new(),
            },
        );
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/research/meta").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["youtube_sources"], serde_json::json!(["yt"]));
    }

    #[tokio::test]
    async fn api_research_meta_reports_auth_configured_from_a_captured_storage_state() {
        let mut config = test_config();
        config.base_dir = temp_base_dir("research-auth");
        std::fs::create_dir_all(config.base_dir.join(".notebooklm")).unwrap();
        std::fs::write(
            config
                .base_dir
                .join(".notebooklm")
                .join("storage_state.json"),
            "{}",
        )
        .unwrap();
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let (status, body) = get_json(router(None, opts), "/api/research/meta").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["auth"]["configured"], true);
    }

    #[tokio::test]
    async fn api_start_research_requires_a_topic() {
        let (status, body) = post_json(
            router(None, test_opts()),
            "/api/research",
            serde_json::json!({"mode": "search", "topic": ""}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("topic"));
    }

    #[tokio::test]
    async fn api_start_research_backup_mode_fails_cleanly_pending_the_notebooklm_adapter() {
        let config = seed_research_youtube_source("backup-mode");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (status, body) = post_json(
            router.clone(),
            "/api/research",
            serde_json::json!({
                "mode": "backup",
                "topic": "my topic",
                "sources": ["yt"],
                "count": 5,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["connector"], "my topic");
        assert_eq!(body["result"], serde_json::Value::Null);
        let id = body["id"].as_u64().unwrap();

        let snap = wait_until_research_job_done(router.clone(), id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"].as_str().unwrap().contains("nlm CLI"));
        assert_eq!(snap["connector"], "my topic");

        // GET .../report has nothing to serve for a failed run.
        let (report_status, _) = get_json(router, &format!("/api/research/{id}/report")).await;
        assert_eq!(report_status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_start_research_backup_mode_errors_when_nothing_matched() {
        let router = router(None, test_opts());
        let (status, body) = post_json(
            router.clone(),
            "/api/research",
            serde_json::json!({"mode": "backup", "topic": "t", "sources": ["nonexistent"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_research_job_done(router, id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"].as_str().unwrap().contains("nothing to send"));
    }

    #[tokio::test]
    async fn api_research_current_is_null_when_nothing_has_run() {
        let (status, body) = get_json(router(None, test_opts()), "/api/research/current").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn api_research_report_404s_for_an_unknown_job() {
        let (status, _) = get_json(router(None, test_opts()), "/api/research/999999/report").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_research_stream_ends_with_the_reshaped_final_snapshot() {
        let config = seed_research_youtube_source("stream");
        let opts = ServeOptions {
            config,
            allow_setup: true,
            schedule: false,
        };
        let router = router(None, opts);
        let (_, body) = post_json(
            router.clone(),
            "/api/research",
            serde_json::json!({"mode": "backup", "topic": "t", "sources": ["yt"]}),
        )
        .await;
        let id = body["id"].as_u64().unwrap();
        wait_until_research_job_done(router.clone(), id).await;

        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/api/research/{id}/stream"))
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sse_body = body_string(response).await;
        assert!(sse_body.contains("event: end"), "{sse_body}");
        assert!(sse_body.contains(r#""status":"error""#), "{sse_body}");
        assert!(sse_body.contains(r#""connector":"t""#), "{sse_body}");
        assert!(!sse_body.contains("\"results\""), "{sse_body}");
    }

    #[tokio::test]
    async fn api_research_install_is_forbidden_when_setup_is_disabled() {
        let opts = ServeOptions {
            config: test_config(),
            allow_setup: false,
            schedule: false,
        };
        let (status, _) = post_json(
            router(None, opts),
            "/api/research/install",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_research_login_is_forbidden_when_setup_is_disabled() {
        let opts = ServeOptions {
            config: test_config(),
            allow_setup: false,
            schedule: false,
        };
        let (status, _) = post_json(
            router(None, opts),
            "/api/research/login",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_research_login_fails_cleanly_and_streams_via_the_shared_setup_mount() {
        let router = router(None, test_opts());
        let (status, body) =
            post_json(router.clone(), "/api/research/login", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_u64().unwrap();
        let snap = wait_until_setup_job_done(router, id).await;
        assert_eq!(snap["status"], "error");
        assert!(snap["error"]
            .as_str()
            .unwrap()
            .contains("dedicated Playwright script"));
    }

    #[tokio::test]
    async fn api_requests_require_the_token_when_one_is_configured() {
        let response = router(Some("secret".to_string()), test_opts())
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header(header::HOST, "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
