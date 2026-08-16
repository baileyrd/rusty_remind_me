//! Coverage for `GET /` (the vendored dashboard) and the CORS policy
//! (`#78`) — confirmed against the reference's own `CORSMiddleware` setup
//! (`allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?"`,
//! `allow_methods=["*"]`, `allow_headers=["*"]`) rather than assumed.

mod common;
use common::{call, call_full, call_with_origin, get, server};

#[test]
fn the_dashboard_route_serves_html_embedding_the_vendored_jsx() {
    let (server, root) = server("dashboard");
    let response = get(&server, "/");
    assert_eq!(response.status, 200);
    assert!(response.content_type.starts_with("text/html"));
    assert!(response.body.contains("<div id=\"root\"></div>"));
    assert!(
        response.body.contains("text/babel"),
        "the JSX must be embedded in a Babel script block"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// Every command endpoint the dashboard drives is both named in the page it
/// serves and answered by the route table behind it.
///
/// The JSX is a string constant to the Rust build: nothing type-checks that a
/// path the page fetches is a path this server routes, so a renamed route
/// would break the dashboard silently and no Rust test would notice. This
/// walks both halves — the literal appears in the served page, and a real
/// request to it is not a 404 — which is the cheapest thing that actually
/// fails when the two drift apart.
#[test]
fn every_command_endpoint_the_dashboard_calls_is_a_real_route() {
    let (server, root) = server("dashboard-endpoints");
    let page = get(&server, "/").body;

    for path in [
        "/reminders",
        "/saved-searches",
        "/digest",
        "/status",
        "/memories",
        "/stats",
        "/vitality",
    ] {
        assert!(
            page.contains(&format!("\"{}", path)),
            "the dashboard no longer references {:?}",
            path
        );
        let response = get(&server, &format!("/api{}", path));
        assert_ne!(
            response.status, 404,
            "the dashboard calls /api{} but nothing routes it",
            path
        );
    }

    // The one path built by concatenation rather than written whole, so the
    // literal check above cannot see it.
    assert!(
        page.contains("\"/saved-searches/\""),
        "the dashboard no longer builds the run-a-saved-search path"
    );
    assert_eq!(
        get(&server, "/api/saved-searches/nothing-saved/run").status,
        404,
        "404 for the absent search, not for an absent route"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_dashboard_is_unauthenticated_even_when_an_api_key_is_configured() {
    // The reference's own Route("/", index) isn't under
    // protect_prefix="/api/" -- serving the dashboard page itself never
    // requires the API key the /api/* routes need.
    let (server, root) = server("dashboard-authed");
    let server = server.with_api_key(Some("s3cret".to_string()));
    let response = get(&server, "/");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_localhost_origin_gets_reflected_cors_headers() {
    let (server, root) = server("cors-localhost");
    let response = call_with_origin(&server, "GET", "/api/stats", "http://localhost:5173");
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:5173")
    );
    assert_eq!(response.header("access-control-allow-methods"), Some("*"));
    assert_eq!(response.header("access-control-allow-headers"), Some("*"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_127_0_0_1_origin_with_no_port_is_also_allowed() {
    let (server, root) = server("cors-loopback");
    let response = call_with_origin(&server, "GET", "/health", "http://127.0.0.1");
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://127.0.0.1")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_foreign_origin_gets_no_cors_headers_at_all() {
    let (server, root) = server("cors-foreign");
    let response = call_with_origin(&server, "GET", "/health", "http://evil.example");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_https_origin_is_rejected_even_on_localhost() {
    // The reference's regex is anchored to the http:// scheme literally.
    let (server, root) = server("cors-https");
    let response = call_with_origin(&server, "GET", "/health", "https://localhost:5173");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_request_with_no_origin_header_gets_no_cors_headers() {
    // The ordinary script/curl case -- CORS is a browser-enforced policy,
    // and nothing here needs it to work non-interactively.
    let (server, root) = server("cors-no-origin");
    let response = get(&server, "/health");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_options_preflight_from_an_allowed_origin_gets_cors_headers_with_no_route_dispatch() {
    let (server, root) = server("cors-preflight");
    let response = call_with_origin(&server, "OPTIONS", "/api/memories", "http://localhost:3000");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:3000")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_options_preflight_from_a_foreign_origin_gets_no_cors_headers() {
    let (server, root) = server("cors-preflight-foreign");
    let response = call_with_origin(&server, "OPTIONS", "/api/memories", "http://evil.example");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cors_headers_are_added_to_error_responses_too() {
    // The reference's CORSMiddleware wraps the whole app, including a 404 --
    // a browser reading a failed request's error still needs the header.
    let (server, root) = server("cors-on-404");
    let response = call_with_origin(&server, "GET", "/not-a-real-path", "http://localhost:1234");
    assert_eq!(response.status, 404);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:1234")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cors_headers_are_added_to_a_401_from_a_missing_api_key_too() {
    let (server, root) = server("cors-on-401");
    let server = server.with_api_key(Some("s3cret".to_string()));
    let response = call_full(
        &server,
        "POST",
        "/api/memories",
        Some("http://localhost:1234"),
        None,
        Some("application/json"),
        "{}",
    );
    assert_eq!(response.status, 401);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:1234")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_dashboard_route_still_works_when_called_through_the_normal_call_helper() {
    let (server, root) = server("dashboard-via-call");
    let response = call(&server, "GET", "/", None, None, "");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
