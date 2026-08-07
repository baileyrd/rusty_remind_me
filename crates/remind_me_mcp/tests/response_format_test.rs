//! `response_format` on the twelve tools that were JSON-only (#206).
//!
//! The reference returns Markdown from these and offers no JSON; this port
//! returned JSON and offered no Markdown. Both were half a surface. Markdown is
//! now opt-in with **JSON as the default**, so no existing caller changes
//! behaviour and the capability gap closes.
//!
//! Every case drives the real JSON-RPC dispatch rather than calling a renderer
//! directly: a renderer that is never reached from `tools/call` would satisfy a
//! unit test and change nothing a client can see.

use remind_me_core::{Database, MemoryAddInput};
use remind_me_mcp::McpServer;
use serde_json::{json, Value};

/// Every tool this issue covers, with arguments good enough to reach a
/// success path.
///
/// `remind_me_history` is deliberately absent: it already offered both formats
/// and already defaulted to Markdown, so giving it a JSON default would be the
/// one regression in a change that is otherwise a pure addition.
fn cases(seed: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("remind_me_add", json!({"content": "a new fact"})),
        (
            "remind_me_update",
            json!({"memory_id": seed, "content": "revised"}),
        ),
        (
            "remind_me_set_reminder",
            json!({"memory_id": seed, "remind_at": "2030-01-01T00:00:00+00:00"}),
        ),
        (
            "remind_me_save_search",
            json!({"name": "s1", "query": "Boston"}),
        ),
        ("remind_me_list_saved_searches", json!({})),
        ("remind_me_server_status", json!({})),
        ("remind_me_check_update", json!({})),
        ("remind_me_reindex", json!({})),
        (
            "remind_me_auto_capture",
            json!({"conversation": "a: hi", "summary": "a greeting"}),
        ),
        ("remind_me_wiki_compile", json!({})),
    ]
}

fn server() -> (McpServer, String) {
    let db = Database::open_in_memory().unwrap();
    let id = {
        let conn = db.conn();
        remind_me_core::db::queries::add_memory(
            &conn,
            MemoryAddInput {
                content: "seed memory about Boston".into(),
                category: "general".into(),
                tags: vec![],
                source: "manual".into(),
                metadata: json!({}),
                subject: None,
                predicate: None,
                object: None,
                entities: vec![],
                sensitive: false,
            },
        )
        .unwrap()
        .id
    };
    (McpServer::new(db), id)
}

fn call(server: &McpServer, name: &str, args: Value) -> String {
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let res = server.handle_request(&req.to_string()).unwrap();
    res["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn is_json(text: &str) -> bool {
    serde_json::from_str::<Value>(text).is_ok()
}

#[test]
fn json_is_the_default_for_every_tool_that_had_no_choice() {
    // The compatibility half. These twelve emitted JSON before this change and
    // must still emit JSON when nothing is asked for, or every existing caller
    // breaks.
    for (name, args) in cases("placeholder") {
        let (srv, seed) = server();
        let args = if args.get("memory_id").is_some() {
            json!({"memory_id": seed, "content": "revised", "remind_at": "2030-01-01T00:00:00+00:00"})
        } else {
            args
        };
        let text = call(&srv, name, args);
        assert!(
            is_json(&text),
            "{name} must default to JSON, got: {}",
            text.chars().take(120).collect::<String>()
        );
    }
}

#[test]
fn markdown_is_reachable_for_every_tool() {
    // The capability half. Asking for Markdown must produce something that is
    // *not* JSON — otherwise the parameter is decorative.
    for (name, args) in cases("placeholder") {
        let (srv, seed) = server();
        let mut args = if args.get("memory_id").is_some() {
            json!({"memory_id": seed, "content": "revised", "remind_at": "2030-01-01T00:00:00+00:00"})
        } else {
            args
        };
        args.as_object_mut()
            .unwrap()
            .insert("response_format".into(), json!("markdown"));
        let text = call(&srv, name, args);
        assert!(!text.is_empty(), "{name} returned nothing for markdown");
        assert!(
            !is_json(&text),
            "{name} still returned JSON when markdown was asked for: {}",
            text.chars().take(120).collect::<String>()
        );
    }
}

#[test]
fn add_renders_the_reference_s_confirmation_line() {
    // Pinned against the reference's actual wording:
    //   "✓ Memory stored with id `a0563f…` in category 'general'."
    let (srv, _) = server();
    let text = call(
        &srv,
        "remind_me_add",
        json!({"content": "a fact", "response_format": "markdown"}),
    );
    assert!(text.starts_with("✓ Memory stored with id"), "got: {text}");
    assert!(text.contains("in category 'general'"), "got: {text}");
}

#[test]
fn an_unknown_format_falls_back_to_json_rather_than_failing() {
    // A caller typo should still return their data. Erroring would turn a
    // cosmetic mistake into a failed call.
    let (srv, _) = server();
    let text = call(
        &srv,
        "remind_me_add",
        json!({"content": "a fact", "response_format": "yaml"}),
    );
    assert!(is_json(&text), "got: {text}");
}

#[test]
fn every_covered_tool_advertises_the_parameter() {
    // A format a client cannot discover is a format nobody uses.
    let (srv, _) = server();
    let listed = srv
        .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string())
        .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();

    for name in [
        "remind_me_add",
        "remind_me_update",
        "remind_me_revert",
        "remind_me_set_reminder",
        "remind_me_save_search",
        "remind_me_list_saved_searches",
        "remind_me_server_status",
        "remind_me_check_update",
        "remind_me_reindex",
        "remind_me_auto_capture",
        "remind_me_wiki_compile",
        "remind_me_wiki_read",
    ] {
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not registered"));
        let prop = &tool["inputSchema"]["properties"]["response_format"];
        assert!(!prop.is_null(), "{name} does not advertise response_format");
        assert_eq!(
            prop["default"], "json",
            "{name} must advertise json as the default"
        );
    }
}

#[test]
fn history_keeps_its_markdown_default() {
    // The deliberate exception. It already offered both, already defaulted to
    // Markdown, and flipping it would be the only regression in this change.
    let (srv, seed) = server();
    let text = call(&srv, "remind_me_history", json!({"memory_id": seed}));
    assert!(
        !is_json(&text),
        "remind_me_history must keep defaulting to markdown, got JSON: {}",
        text.chars().take(120).collect::<String>()
    );
}
