//! Secret-path / bearer gate for the remote MCP connector (FT-05).
//!
//! Ported from the reference's `SecretPathMiddleware` (`remind_me_mcp/remote.py`),
//! legacy/no-OAuth branch only. Admits a request when either:
//!
//! - its path is `/mcp/<token>` (optionally with a trailing segment) — the
//!   path is rewritten to `/mcp` (or `/mcp/<rest>`) and forwarded; or
//! - its path is exactly `/mcp` and carries `Authorization: Bearer <token>`.
//!
//! `/health` always passes through unauthenticated (SE-04 parity). Every
//! other path is answered 404 without distinguishing "wrong token" from
//! "not a real path" — a path-based probe learns nothing. A syntactically
//! plausible `/mcp/<token>` with the wrong token is also 404, matching the
//! reference exactly (only the header-based `/mcp` form uses 401, since that
//! form has no ambiguity about which endpoint it targets).
//!
//! Implemented as an axum middleware ([`secret_gate`], via
//! `axum::middleware::from_fn_with_state`) applied at the `Router` level, so
//! it runs — and can rewrite [`http::Uri`] — before the inner router matches
//! a route, the same ordering the reference's ASGI middleware relies on.
//!
//! The token comparison reuses [`remind_me_core::webhook::constant_time_eq`]
//! rather than a new implementation next to it, per this crate's own
//! precedent (`remind_me_api`'s `http.rs`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::uri::PathAndQuery;
use axum::http::{header, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use remind_me_core::webhook::constant_time_eq;
use serde_json::json;

/// The one path the MCP transport is mounted at. Not configurable: nothing
/// in this crate exposes a different mount point, unlike the reference
/// (which derives it from `mcp.settings.streamable_http_path`).
pub const MCP_PATH: &str = "/mcp";

/// The health probe path, always unauthenticated.
pub const HEALTH_PATH: &str = "/health";

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Not found" }))).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

/// Rewrite `uri`'s path to `new_path`, preserving any query string.
///
/// `None` only if `new_path` (built entirely from this module's own
/// constants and validated path segments) somehow fails to parse as a valid
/// `PathAndQuery` -- callers treat that as a 404 rather than panicking, but
/// it is not expected to occur in practice.
fn rewrite_path(uri: &Uri, new_path: &str) -> Option<Uri> {
    let mut parts = uri.clone().into_parts();
    let path_and_query = match uri.query() {
        Some(query) => format!("{new_path}?{query}"),
        None => new_path.to_string(),
    };
    parts.path_and_query = Some(PathAndQuery::try_from(path_and_query).ok()?);
    Uri::from_parts(parts).ok()
}

/// The axum middleware itself. `token` is the resolved connector token
/// (never logged, never included in a response).
pub async fn secret_gate(
    State(token): State<Arc<String>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    if path == HEALTH_PATH {
        return next.run(request).await;
    }

    let prefix = format!("{MCP_PATH}/");
    if let Some(after_prefix) = path.strip_prefix(&prefix) {
        let (segment, rest) = match after_prefix.split_once('/') {
            Some((segment, rest)) => (segment, Some(rest)),
            None => (after_prefix, None),
        };
        if segment.is_empty() || !constant_time_eq(segment.as_bytes(), token.as_bytes()) {
            return not_found();
        }
        let new_path = match rest {
            Some(rest) if !rest.is_empty() => format!("{MCP_PATH}/{rest}"),
            _ => MCP_PATH.to_string(),
        };
        return match rewrite_path(request.uri(), &new_path) {
            Some(rewritten) => {
                *request.uri_mut() = rewritten;
                next.run(request).await
            }
            None => not_found(),
        };
    }

    if path == MCP_PATH {
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let expected = format!("Bearer {token}");
        return if constant_time_eq(authorization.as_bytes(), expected.as_bytes()) {
            next.run(request).await
        } else {
            unauthorized()
        };
    }

    not_found()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_path_preserves_an_existing_query_string() {
        let uri: Uri = "/mcp/sekrit?foo=bar".parse().unwrap();
        let rewritten = rewrite_path(&uri, MCP_PATH).unwrap();
        assert_eq!(rewritten, "/mcp?foo=bar");
    }

    #[test]
    fn rewrite_path_with_no_query_string() {
        let uri: Uri = "/mcp/sekrit".parse().unwrap();
        let rewritten = rewrite_path(&uri, MCP_PATH).unwrap();
        assert_eq!(rewritten, "/mcp");
    }
}
