//! Security gate for the web app: local-only auth, Origin/Host header
//! validation (DNS-rebinding + CSRF defense), and an opt-in bearer
//! token — mirrors the reference's `_security_gate` middleware and its
//! `_host_is_local`/`_origin_is_local`/`_token_ok` helpers in
//! `dbs.web.app` (issue #81).
//!
//! Three checks, applied to every request in order:
//!
//! 1. **Host header** (DNS-rebinding defense): a webpage that tricks a
//!    victim's browser into resolving an attacker-controlled hostname
//!    to `127.0.0.1` reaches this server with that hostname in `Host`.
//!    Without a `--token` configured, only loopback names are accepted
//!    — with one, the token is the gate instead (a real deployment
//!    binding off-loopback needs a real hostname in `Host`, which this
//!    check would otherwise always reject).
//! 2. **Origin header** (CSRF defense) on state-changing requests
//!    (anything but `GET`/`HEAD`/`OPTIONS`) that aren't already
//!    token-authenticated: browsers always send `Origin` on a
//!    cross-origin request; a same-origin request either omits it or
//!    sends a local one. There's no server-side session to bind a
//!    synchronizer token to (the bearer token already serves that
//!    role when configured), so this is the reference's actual CSRF
//!    defense — not a separate issued-and-validated token.
//! 3. **Bearer token**, once `--token` is configured: required on
//!    every `/api` request (`Authorization: Bearer <token>` or a
//!    `?token=` query parameter, for `EventSource`/download links that
//!    can't set headers). The static SPA itself stays reachable
//!    without one so it can load and prompt for the token.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

/// Hostnames/origins treated as "this machine" — matches the
/// reference's `_LOCAL_HOSTS` exactly, including `testserver`
/// (Starlette's `TestClient` default host; harmless to accept here too
/// since a real attacker can't get an internet-resolvable domain to
/// present as `testserver`).
const LOCAL_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1", "testserver"];

/// What the security gate enforces for this server instance. `token`
/// mirrors `dbs serve --token`; `None` means the DNS-rebinding Host
/// check is strict (loopback only) and no `/api` request needs a
/// token.
#[derive(Clone, Default)]
pub struct SecurityConfig {
    pub token: Option<String>,
}

/// The `Host` header's hostname, local-ness per [`LOCAL_HOSTS`].
/// Strips a port (`host:port`) or brackets (`[::1]:port`) first, same
/// as the reference's `_host_is_local`.
fn host_is_local(host_header: &str) -> bool {
    let host = host_header.trim().to_ascii_lowercase();
    let host = if let Some(rest) = host.strip_prefix('[') {
        // Bracketed IPv6, e.g. "[::1]:8000" — everything up to the
        // closing bracket.
        rest.split(']').next().unwrap_or(rest)
    } else if host.matches(':').count() == 1 {
        // "name:port" — a bare (unbracketed) IPv6 address has more
        // than one colon, so this doesn't misfire on one.
        host.split(':').next().unwrap_or(&host)
    } else {
        &host
    };
    LOCAL_HOSTS.contains(&host)
}

/// The `Origin` header's hostname, local-ness per [`LOCAL_HOSTS`]. An
/// unparsable Origin is treated as not local (fails closed), same as
/// the reference's `except ValueError: return False`.
fn origin_is_local(origin: &str) -> bool {
    url::Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(|h| LOCAL_HOSTS.contains(&h)))
        .unwrap_or(false)
}

/// Constant-time string comparison — mirrors the reference's use of
/// `hmac.compare_digest` to compare the supplied token against the
/// configured one without leaking its length-prefix match via timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if raw.len() < 7 || !raw[..7].eq_ignore_ascii_case("bearer ") {
        return None;
    }
    Some(raw[7..].trim().to_string())
}

fn query_token(uri: &axum::http::Uri) -> Option<String> {
    let query = uri.query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

/// `true` iff `configured` is set and the request supplies it (as a
/// bearer header, falling back to a `?token=` query parameter),
/// matching exactly. `false` whenever no token is configured at all —
/// mirrors the reference's `_token_ok`.
fn token_ok(
    headers: &axum::http::HeaderMap,
    uri: &axum::http::Uri,
    configured: Option<&str>,
) -> bool {
    let Some(configured) = configured else {
        return false;
    };
    let supplied = bearer_token(headers).or_else(|| query_token(uri));
    match supplied {
        Some(supplied) if !supplied.is_empty() => constant_time_eq(&supplied, configured),
        _ => false,
    }
}

fn json_error(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({ "detail": detail }))).into_response()
}

/// The security gate, as an [`axum::middleware::from_fn_with_state`]
/// middleware function.
pub async fn security_gate(
    State(config): State<Arc<SecurityConfig>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if config.token.is_none() && !host_is_local(host) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "unrecognized Host header (DNS-rebinding defense) — serve with --token to use a \
             non-local hostname",
        );
    }

    let authenticated = token_ok(request.headers(), request.uri(), config.token.as_deref());

    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && !authenticated
    {
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            if !origin_is_local(origin) {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "cross-origin requests are not allowed",
                );
            }
        }
    }

    if config.token.is_some() && request.uri().path().starts_with("/api") && !authenticated {
        return json_error(StatusCode::UNAUTHORIZED, "missing or invalid token");
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// A tiny app with one `GET`/`POST` route under `/api` and one
    /// outside it, gated by [`security_gate`] — enough to exercise all
    /// three checks over real HTTP without needing a real job/backup
    /// route to hang the test on (same "fake" pattern issue #80's tests
    /// use).
    fn app(config: SecurityConfig) -> Router {
        let config = Arc::new(config);
        Router::new()
            .route("/", get(|| async { "index" }))
            .route(
                "/api/thing",
                get(|| async { "ok" }).post(|| async { "posted" }),
            )
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                security_gate,
            ))
            .with_state(config)
    }

    async fn status(router: Router, request: axum::http::Request<Body>) -> StatusCode {
        router.oneshot(request).await.unwrap().status()
    }

    async fn body_of(router: Router, request: axum::http::Request<Body>) -> String {
        let response = router.oneshot(request).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_local_get_with_no_token_configured_succeeds() {
        let req = axum::http::Request::builder()
            .uri("/")
            .header(header::HOST, "127.0.0.1:8000")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(app(SecurityConfig::default()), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_dns_rebound_host_is_rejected_when_no_token_is_configured() {
        let req = axum::http::Request::builder()
            .uri("/")
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .unwrap();
        let body = body_of(app(SecurityConfig::default()), req).await;
        assert!(body.contains("DNS-rebinding"), "{body}");
    }

    #[tokio::test]
    async fn a_non_local_host_is_accepted_once_a_token_is_configured() {
        let req = axum::http::Request::builder()
            .uri("/")
            .header(header::HOST, "dbs.example.com")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_cross_origin_post_without_a_token_is_rejected() {
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(app(SecurityConfig::default()), req).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn a_same_origin_post_succeeds() {
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::ORIGIN, "http://127.0.0.1:8000")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(app(SecurityConfig::default()), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_post_with_no_origin_header_is_accepted_local_tools_omit_it() {
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(app(SecurityConfig::default()), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_api_request_with_no_token_is_rejected_once_one_is_configured() {
        let req = axum::http::Request::builder()
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_api_request_with_the_wrong_bearer_token_is_rejected() {
        let req = axum::http::Request::builder()
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_api_request_with_the_right_bearer_token_succeeds() {
        let req = axum::http::Request::builder()
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_api_request_with_the_token_as_a_query_param_succeeds() {
        let req = axum::http::Request::builder()
            .uri("/api/thing?token=secret")
            .header(header::HOST, "127.0.0.1:8000")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_token_authenticated_post_bypasses_the_origin_check_too() {
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/api/thing")
            .header(header::HOST, "127.0.0.1:8000")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_static_frontend_stays_reachable_without_a_token() {
        let req = axum::http::Request::builder()
            .uri("/")
            .header(header::HOST, "127.0.0.1:8000")
            .body(Body::empty())
            .unwrap();
        let config = SecurityConfig {
            token: Some("secret".to_string()),
        };
        assert_eq!(status(app(config), req).await, StatusCode::OK);
    }

    #[test]
    fn host_is_local_handles_ports_and_bracketed_ipv6() {
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("127.0.0.1:8000"));
        assert!(host_is_local("localhost:8000"));
        assert!(host_is_local("[::1]:8000"));
        assert!(host_is_local("::1"));
        assert!(!host_is_local("attacker.example"));
        assert!(!host_is_local(""));
    }

    #[test]
    fn origin_is_local_handles_scheme_and_port() {
        assert!(origin_is_local("http://127.0.0.1:8000"));
        assert!(origin_is_local("http://localhost"));
        assert!(!origin_is_local("https://evil.example"));
        assert!(!origin_is_local("not a url"));
    }

    #[test]
    fn constant_time_eq_matches_ordinary_string_equality() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "wrong"));
        assert!(!constant_time_eq("secret", "secretlonger"));
    }
}
