//! Web app skeleton for `dbs serve` (issue #79), mirroring the shape of
//! the reference's `dbs.web.app.create_app` — minus everything that
//! hangs off of it (auth gate, in-UI setup, API routes), which are
//! separate, later issues (#81/#83). The [`jobs`] module (issue #80)
//! adds the background-job manager + SSE primitive those issues build
//! on. Today this crate root only serves the static single-page app:
//! `GET /` renders `index.html` with its `{{v}}` cache-bust placeholder
//! substituted (mirroring the reference's `index()` route), and
//! `GET /static/<name>` serves the SPA's other assets.
//!
//! The static assets themselves (`static/index.html`, `static/app.js`,
//! `static/style.css`) are an unmodified copy of the reference's
//! `dbs/web/static/*` — a hand-written vanilla-JS SPA, not a build
//! pipeline output. Its API calls (`fetch("/api/...")`) won't resolve
//! against anything yet: every route it drives is a later web-tier
//! issue. Shipping it now, unmodified, means each of those issues lands
//! against a frontend that's already real instead of needing its own
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
//! [`serve`]. Nothing in this skeleton calls into `dbs-core`/`Storage`
//! yet (there are no API routes to need it), so there's no blocking
//! call to bridge today — but the decision for when that need arrives
//! (job manager, auth, in-UI setup) is made here, once, rather than
//! per-issue: async handlers cross into `dbs-core`'s synchronous,
//! `&mut dyn Storage`-based API via `tokio::task::spawn_blocking` at
//! the call site, not by growing `dbs-core` an async-facing wrapper.
//! `dbs-core` has no reason to know Tokio exists.

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod jobs;

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

#[derive(Clone)]
struct AppState {
    /// Substituted for `index.html`'s `{{v}}` placeholder, cache-busting
    /// `/static` asset links on every process restart. The reference
    /// derives this from the static files' on-disk mtimes; these assets
    /// are compiled into the binary rather than read from disk at
    /// runtime, so there's no mtime to read — the process start time
    /// serves the same purpose (changes exactly when the served assets
    /// could have changed, i.e. on every new build/restart).
    cache_stamp: String,
}

fn cache_stamp_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Builds the app skeleton's router: `GET /` (the SPA shell) and
/// `GET /static/<name>` (its JS/CSS). No `/api` routes yet — see the
/// module doc-comment.
pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/:name", get(static_asset))
        .with_state(AppState {
            cache_stamp: cache_stamp_now(),
        })
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
pub async fn serve(host: &str, port: u16) -> std::io::Result<()> {
    let host = if host.is_empty() || host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    axum::serve(listener, router()).await
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

    #[tokio::test]
    async fn index_serves_the_spa_shell_with_the_cache_bust_placeholder_substituted() {
        let response = router()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
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
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/static/app.js")
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
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/static/style.css")
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
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/static/nope.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_route_is_a_404() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
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
            axum::serve(listener, router()).await.unwrap();
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
}
