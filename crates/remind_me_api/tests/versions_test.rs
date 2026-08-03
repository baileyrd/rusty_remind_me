//! Coverage for `GET /api/versions` and the version on `/health` (issue #107).
//!
//! The split between the two routes is deliberate and is the thing most worth
//! pinning: this node's own build is published unauthenticated on `/health`,
//! because a wrong or missing API key is exactly when you most want to know
//! which build you are talking to. The *hub's* build is another machine's, so
//! it sits behind the `/api/` prefix's auth.

mod common;
use common::{authed_get, authed_server, get, server};

#[test]
fn health_reports_the_serving_build_without_a_key() {
    let (srv, root) = authed_server("versions-health");

    // No Authorization header. This is the point of putting it here.
    let response = get(&srv, "/health");

    assert_eq!(response.status, 200);
    assert_eq!(response.json()["status"], "ok");
    assert_eq!(response.json()["version"], env!("CARGO_PKG_VERSION"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn api_versions_reports_the_node_build_and_sync_state() {
    let (srv, root) = server("versions-shape");

    let response = get(&srv, "/api/versions");

    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    // Sync is unconfigured in tests, so both of these pin the "off" shape:
    // `hub` must be null rather than absent, so the dashboard can distinguish
    // "no hub" from "field missing" without guessing.
    assert_eq!(body["sync_enabled"], false);
    assert!(body["hub"].is_null());
    assert!(
        body.get("hub").is_some(),
        "hub must be present-and-null, not omitted"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unconfigured_node_id_is_null_rather_than_an_empty_string() {
    let (srv, root) = server("versions-node-id");

    let body = get(&srv, "/api/versions").json();

    // `""` in a UI reads as a rendering bug; null reads as "not set", which is
    // what it means.
    assert!(
        body["node_id"].is_null(),
        "expected null, got {:?}",
        body["node_id"]
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn api_versions_is_behind_auth_unlike_health() {
    let (srv, root) = authed_server("versions-auth");

    // Unauthenticated: refused, because the hub's build is not this node's to
    // publish. `/health` above is deliberately the opposite.
    assert_eq!(get(&srv, "/api/versions").status, 401);
    assert_eq!(authed_get(&srv, "/api/versions").status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
