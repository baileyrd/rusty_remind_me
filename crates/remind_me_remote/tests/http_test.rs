//! Integration tests driving the real axum/rmcp server over HTTP.
//!
//! These exercise `remind_me_remote::build_router` exactly as production
//! uses it (default `StreamableHttpServerConfig`, so real session-managed
//! SSE, not the `with_json_response(true)` shortcut some of rmcp's own
//! tests use) -- what a real claude.ai connector would actually receive.
//! See `crate::handler`'s module doc and this crate's ADR for why unit
//! tests alone can't cover `ServerHandler` dispatch (its `RequestContext`
//! has no public constructor outside `rmcp`): this file is where that
//! coverage lives instead.

use std::net::SocketAddr;
use std::sync::Arc;

use remind_me_core::Database;
use remind_me_mcp::McpServer;
use serde_json::{json, Value};

const TOKEN: &str = "test-connector-token";

/// Spin up `build_router` on an ephemeral loopback port and return its
/// address plus a cancellation-free background task -- the listener and
/// server both drop, and the OS reclaims the port, when the test ends.
async fn spawn_server() -> SocketAddr {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router = remind_me_remote::build_router(mcp, TOKEN.to_string(), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// Extract every JSON payload carried by an SSE response body's `data:`
/// fields -- rmcp's real wire format, not a JSON-RPC body directly. The
/// first event is typically an empty-data retry/priming event; that's
/// filtered out here rather than asserted on, since its presence isn't
/// something this crate controls.
fn sse_json_payloads(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|event| {
            let data: String = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                None
            } else {
                serde_json::from_str(&data).ok()
            }
        })
        .collect()
}

fn mcp_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers.insert(
        "Accept",
        "application/json, text/event-stream".parse().unwrap(),
    );
    headers
}

#[tokio::test]
async fn health_probe_is_unauthenticated_and_reveals_no_data() {
    let addr = spawn_server().await;
    let response = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, json!({ "status": "ok" }));
}

#[tokio::test]
async fn mcp_without_any_credential_is_rejected_with_401() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, json!({ "error": "Unauthorized" }));
}

#[tokio::test]
async fn mcp_with_the_wrong_bearer_token_is_rejected_with_401() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .header("Authorization", "Bearer not-the-real-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_wrong_secret_path_token_404s_identically_to_an_unrelated_path() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let wrong_token = client
        .get(format!("http://{addr}/mcp/not-the-real-token"))
        .send()
        .await
        .unwrap();
    let unrelated = client
        .get(format!("http://{addr}/totally/unrelated"))
        .send()
        .await
        .unwrap();

    assert_eq!(wrong_token.status(), 404);
    assert_eq!(unrelated.status(), 404);
    // Same status AND same body: a probe cannot tell "wrong token" apart
    // from "this path was never a thing".
    assert_eq!(
        wrong_token.text().await.unwrap(),
        unrelated.text().await.unwrap()
    );
}

#[tokio::test]
async fn an_empty_secret_path_segment_404s_rather_than_matching_the_empty_string() {
    let addr = spawn_server().await;
    let response = reqwest::get(format!("http://{addr}/mcp/")).await.unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn secret_path_initialize_negotiates_a_real_session_managed_sse_stream() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0" }
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "the secret-path form must reach the real rmcp session transport, \
         not a plain JSON responder"
    );
    assert!(
        response.headers().contains_key("mcp-session-id"),
        "a session-managed (legacy_session_mode default) server must hand \
         back a session id on initialize"
    );

    let body = response.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let init_result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the initialize response must be among the SSE payloads");
    assert_eq!(
        init_result["result"]["serverInfo"]["name"],
        "rusty_remind_me"
    );
    assert!(init_result["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn bearer_auth_reuses_a_session_the_secret_path_opened_and_round_trips_a_real_tool_call() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    // Open the session via the secret-path form (no Authorization header).
    let init = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0" }
            }
        }))
        .send()
        .await
        .unwrap();
    let session_id = init
        .headers()
        .get("mcp-session-id")
        .expect("initialize must return a session id")
        .to_str()
        .unwrap()
        .to_string();
    let _ = init.text().await.unwrap();

    // Required before further requests on a freshly initialized session.
    let initialized_ack = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .unwrap();
    assert!(initialized_ack.status().is_success());

    // Now call a real tool, over the bearer form, reusing the session the
    // secret-path form opened -- proving the two ways in are interchangeable
    // within one session, exactly as the reference documents.
    let call = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "remind_me_add",
                "arguments": { "content": "written through the remote transport" }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = call.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(2)))
        .expect("the tools/call response must be among the SSE payloads");
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .expect("call_tool result content must carry the stored memory as text");
    assert!(
        text.contains("written through the remote transport"),
        "expected the round-tripped memory content in the tool result, got: {text}"
    );
    assert_ne!(result["result"]["isError"], json!(true));
}
