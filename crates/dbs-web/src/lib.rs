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
    let state = AppState {
        cache_stamp: cache_stamp_now(),
        config: Arc::new(opts.config),
        allow_setup: opts.allow_setup,
        scheduler_enabled: opts.schedule,
    };
    Router::new()
        .route("/", get(index))
        .route("/static/:name", get(static_asset))
        .merge(api::router())
        .layer(axum::middleware::from_fn_with_state(
            security_config,
            auth::security_gate,
        ))
        .with_state(state)
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
        assert!(body["error"].as_str().unwrap().contains("no such item"));
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
    async fn api_verify_reports_not_implemented() {
        let (status, body) = get_json(router(None, test_opts()), "/api/verify").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("not yet implemented"));
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
