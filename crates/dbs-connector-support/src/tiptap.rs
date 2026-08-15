//! TipTap/ProseMirror rich-text JSON → Markdown, best-effort.
//!
//! Mirrors `connectors/_tiptap.py` (pinned `@6cc6491`) node-for-node.
//! Skool (and potentially other TipTap-backed sources) stores rich text
//! as `[v2]{...tiptap json...}` — older content may carry plain text
//! instead. [`tiptap_markdown`] converts the common node types to
//! GitHub-flavored markdown and passes anything it can't decode through
//! unchanged: a rich-text body must never fail a backup, and the
//! verbatim payload is always preserved separately in the item's `raw`
//! regardless of what this renders.

use serde_json::{Map, Value};

const V2_PREFIX: &str = "[v2]";

/// Recursion cap for `blocks`/`block`/`list_items`/`inline`/`inline_node`,
/// which are mutually recursive over an untrusted TipTap document with no
/// depth limit otherwise — a maliciously or accidentally deeply-nested
/// `desc` (a few thousand nested containers, an easily-craftable payload)
/// would otherwise exhaust the thread stack and abort the process. Deep
/// enough for any real document; content nested past this is dropped
/// (best-effort rendering, same philosophy as an undecodable payload).
const MAX_DEPTH: usize = 64;

/// Escapes `]` in markdown link text — unescaped, it closes the link
/// early and mangles everything after it in the rendered note.
fn md_link_text(text: &str) -> String {
    text.replace(']', "\\]")
}

/// Renders a TipTap-formatted description to markdown, best-effort.
///
/// `desc` is typically a JSON string value pulled straight out of a
/// connector's raw record (e.g. Skool's lesson `desc` field). A
/// `"[v2]{...tiptap json...}"`-prefixed string is decoded node-by-node;
/// a bare JSON document (no prefix) converts too; anything that isn't a
/// non-empty string, or that fails to decode, is returned as-is (the
/// prefix stripped off first if present, matching the reference).
pub fn tiptap_markdown(desc: &Value) -> String {
    let Some(raw) = desc.as_str() else {
        return String::new();
    };
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    let has_prefix = text.starts_with(V2_PREFIX);
    let payload = if has_prefix {
        &text[V2_PREFIX.len()..]
    } else {
        text
    };
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Array(nodes)) => blocks(&nodes, 0).trim().to_string(),
        Ok(Value::Object(map)) => {
            let content = map
                .get("content")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            blocks(&content, 0).trim().to_string()
        }
        Ok(_) => raw.to_string(),
        Err(_) if has_prefix => payload.to_string(),
        Err(_) => raw.to_string(),
    }
}

// -- node rendering -----------------------------------------------------

fn blocks(nodes: &[Value], depth: usize) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    nodes
        .iter()
        .filter_map(|n| n.as_object())
        .map(|n| block(n, depth + 1))
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn block(node: &Map<String, Value>, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    let kind = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let content: Vec<Value> = node
        .get("content")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let empty_attrs = Map::new();
    let attrs = node
        .get("attrs")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty_attrs);
    match kind {
        "paragraph" => inline(&content, depth + 1),
        "heading" => {
            let level = attrs
                .get("level")
                .and_then(|v| v.as_i64())
                .filter(|l| (1..=6).contains(l))
                .unwrap_or(1);
            format!(
                "{} {}",
                "#".repeat(level as usize),
                inline(&content, depth + 1)
            )
        }
        "codeBlock" => {
            let lang = attrs.get("language").and_then(|v| v.as_str()).unwrap_or("");
            let code: String = content
                .iter()
                .filter_map(|n| n.as_object())
                .map(|n| n.get("text").and_then(|v| v.as_str()).unwrap_or(""))
                .collect();
            format!("```{lang}\n{code}\n```")
        }
        "blockquote" => {
            let inner = blocks(&content, depth + 1);
            inner
                .split('\n')
                .map(|line| {
                    if line.is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "bulletList" => list_items(&content, depth + 1, |_i| "- ".to_string()),
        "orderedList" => {
            let start = attrs.get("start").and_then(|v| v.as_i64()).unwrap_or(1);
            list_items(&content, depth + 1, move |i| {
                format!("{}. ", start + i as i64)
            })
        }
        "horizontalRule" => "---".to_string(),
        "image" => {
            let alt = attrs.get("alt").and_then(|v| v.as_str()).unwrap_or("");
            let src = attrs.get("src").and_then(|v| v.as_str()).unwrap_or("");
            format!("![{alt}]({src})")
        }
        // A stray inline node at block level.
        "text" => inline_node(node, depth + 1),
        // Unknown container (tables, embeds, ...): render what's inside.
        _ => {
            if content.is_empty() {
                String::new()
            } else {
                blocks(&content, depth + 1)
            }
        }
    }
}

fn list_items(nodes: &[Value], depth: usize, bullet: impl Fn(usize) -> String) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut idx = 0usize;
    for item in nodes {
        let Some(item) = item.as_object() else {
            continue;
        };
        let content: Vec<Value> = item
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let inner = blocks(&content, depth + 1);
        if inner.is_empty() {
            continue;
        }
        let prefix = bullet(idx);
        let mut rest = inner.split('\n');
        let first = rest.next().unwrap_or("");
        lines.push(format!("{prefix}{first}"));
        let pad = " ".repeat(prefix.chars().count());
        for line in rest {
            lines.push(if line.is_empty() {
                String::new()
            } else {
                format!("{pad}{line}")
            });
        }
        idx += 1;
    }
    lines.join("\n")
}

fn inline(nodes: &[Value], depth: usize) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    nodes
        .iter()
        .filter_map(|n| n.as_object())
        .map(|n| inline_node(n, depth + 1))
        .collect()
}

fn inline_node(n: &Map<String, Value>, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    let kind = n.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if kind == "hardBreak" {
        return "\n".to_string();
    }
    if kind == "image" {
        let empty = Map::new();
        let attrs = n.get("attrs").and_then(|v| v.as_object()).unwrap_or(&empty);
        let alt = attrs.get("alt").and_then(|v| v.as_str()).unwrap_or("");
        let src = attrs.get("src").and_then(|v| v.as_str()).unwrap_or("");
        return format!("![{alt}]({src})");
    }
    if kind != "text" {
        let content: Vec<Value> = n
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        return inline(&content, depth + 1);
    }
    let mut text = n
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut link: Option<String> = None;
    if let Some(marks) = n.get("marks").and_then(|v| v.as_array()) {
        for mark in marks {
            let Some(mark) = mark.as_object() else {
                continue;
            };
            match mark.get("type").and_then(|v| v.as_str()) {
                Some("bold") => text = format!("**{text}**"),
                Some("italic") => text = format!("*{text}*"),
                Some("code") => text = format!("`{text}`"),
                Some("strike") => text = format!("~~{text}~~"),
                Some("link") => {
                    link = mark
                        .get("attrs")
                        .and_then(|v| v.as_object())
                        .and_then(|a| a.get("href"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                _ => {}
            }
        }
    }
    if let Some(href) = link {
        text = format!("[{}]({href})", md_link_text(&text));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(content: Vec<Value>) -> Value {
        json!(format!(
            "[v2]{}",
            serde_json::to_string(&json!({"type": "doc", "content": content})).unwrap()
        ))
    }

    fn p(content: Vec<Value>) -> Value {
        json!({"type": "paragraph", "content": content})
    }

    fn t(text: &str, marks: Vec<Value>) -> Value {
        if marks.is_empty() {
            json!({"type": "text", "text": text})
        } else {
            json!({"type": "text", "text": text, "marks": marks})
        }
    }

    fn mark(kind: &str) -> Value {
        json!({"type": kind})
    }

    #[test]
    fn paragraphs_headings_and_marks() {
        let desc = doc(vec![
            json!({"type": "heading", "attrs": {"level": 2}, "content": [t("Setup", vec![])]}),
            p(vec![
                t("Use ", vec![]),
                t("bold", vec![mark("bold")]),
                t(" and ", vec![]),
                t("code", vec![mark("code")]),
                t(".", vec![]),
            ]),
            p(vec![t(
                "docs",
                vec![json!({"type": "link", "attrs": {"href": "https://x.dev"}})],
            )]),
        ]);
        assert_eq!(
            tiptap_markdown(&desc),
            "## Setup\n\nUse **bold** and `code`.\n\n[docs](https://x.dev)"
        );
    }

    #[test]
    fn link_text_with_brackets_is_escaped() {
        let desc = doc(vec![p(vec![t(
            "See [this]",
            vec![json!({"type": "link", "attrs": {"href": "https://x.dev"}})],
        )])]);
        assert_eq!(tiptap_markdown(&desc), "[See [this\\]](https://x.dev)");
    }

    #[test]
    fn lists_code_blocks_and_quotes() {
        let desc = doc(vec![
            json!({"type": "bulletList", "content": [
                {"type": "listItem", "content": [p(vec![t("first", vec![])])]},
                {"type": "listItem", "content": [p(vec![t("second", vec![])])]},
            ]}),
            json!({"type": "orderedList", "content": [
                {"type": "listItem", "content": [p(vec![t("step", vec![])])]},
            ]}),
            json!({"type": "codeBlock", "attrs": {"language": "bash"},
                   "content": [t("echo hi", vec![])]}),
            json!({"type": "blockquote", "content": [p(vec![t("wise words", vec![])])]}),
            json!({"type": "horizontalRule"}),
        ]);
        assert_eq!(
            tiptap_markdown(&desc),
            "- first\n- second\n\n1. step\n\n```bash\necho hi\n```\n\n> wise words\n\n---"
        );
    }

    #[test]
    fn images_hard_breaks_and_nested_lists() {
        let desc = doc(vec![
            p(vec![
                t("line one", vec![]),
                json!({"type": "hardBreak"}),
                t("line two", vec![]),
            ]),
            json!({"type": "image", "attrs": {"src": "https://img/x.png", "alt": "shot"}}),
            json!({"type": "bulletList", "content": [
                {"type": "listItem", "content": [
                    p(vec![t("outer", vec![])]),
                    {"type": "bulletList", "content": [
                        {"type": "listItem", "content": [p(vec![t("inner", vec![])])]},
                    ]},
                ]},
            ]}),
        ]);
        let out = tiptap_markdown(&desc);
        assert!(out.contains("line one\nline two"));
        assert!(out.contains("![shot](https://img/x.png)"));
        assert!(out.contains("- outer\n") && out.contains("  - inner"));
    }

    #[test]
    fn unknown_nodes_render_their_children() {
        let desc = doc(vec![
            json!({"type": "customEmbed", "content": [p(vec![t("still here", vec![])])]}),
        ]);
        assert_eq!(tiptap_markdown(&desc), "still here");
    }

    #[test]
    fn passthrough_and_garbage() {
        assert_eq!(
            tiptap_markdown(&json!("Just some notes")),
            "Just some notes"
        );
        assert_eq!(tiptap_markdown(&json!("[v2]not-json{")), "not-json{");
        let bare =
            serde_json::to_string(&json!({"type": "doc", "content": [p(vec![t("hi", vec![])])]}))
                .unwrap();
        assert_eq!(tiptap_markdown(&json!(bare)), "hi");
        assert_eq!(tiptap_markdown(&Value::Null), "");
        assert_eq!(tiptap_markdown(&json!("")), "");
        let garbage = format!(
            "[v2]{}",
            serde_json::to_string(&json!(["not", "a", "doc"])).unwrap()
        );
        assert_eq!(tiptap_markdown(&json!(garbage)), "");
    }

    #[test]
    fn bare_block_array_without_doc_wrapper() {
        let bare_array = json!([p(vec![t("first", vec![])]), p(vec![t("second", vec![])])]);
        let payload = format!("[v2]{}", serde_json::to_string(&bare_array).unwrap());
        assert_eq!(tiptap_markdown(&json!(payload)), "first\n\nsecond");
    }

    #[test]
    fn a_maliciously_deep_raw_payload_does_not_crash_tiptap_markdown() {
        // Builds the raw JSON text directly (string concatenation, not a
        // nested serde_json::Value -- constructing/serializing/dropping a
        // Value nested this deep is its own, unrelated stack-overflow risk
        // in serde_json itself, orthogonal to what this test is checking).
        // 10,000 levels is far deeper than any real Skool lesson could be,
        // easily craftable by a malicious/compromised community's `desc`.
        let depth = 10_000;
        let mut payload = String::from("[v2]");
        payload.push_str(&"{\"type\":\"blockquote\",\"content\":[".repeat(depth));
        payload
            .push_str("{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"x\"}]}");
        payload.push_str(&"]}".repeat(depth));
        // Must return -- not crash -- regardless of whether serde_json's
        // own parser recursion limit rejects the payload first (falling
        // back to the raw text) or a shallower-but-still-deep payload
        // reaches this module's own MAX_DEPTH-capped render functions.
        let _ = tiptap_markdown(&json!(payload));
    }

    #[test]
    fn blocks_stops_recursing_once_the_depth_cap_is_exceeded() {
        // Exercises the guard clause directly, without needing to
        // construct (or later drop) a genuinely deep serde_json::Value
        // tree -- a real payload can never parse deep enough to reach
        // MAX_DEPTH through tiptap_markdown's own from_str call (serde_json
        // rejects anything that deep well before this module's recursive
        // functions would), but the cap is still explicit, connector-owned
        // defense-in-depth rather than an implicit reliance on that
        // upstream default.
        let nodes = vec![p(vec![t("should be dropped", vec![])])];
        let rendered = blocks(&nodes, MAX_DEPTH + 1);
        assert_eq!(rendered, "");
    }

    #[test]
    fn nesting_within_the_depth_cap_still_renders_correctly() {
        let mut node = p(vec![t("innermost", vec![])]);
        for _ in 0..10 {
            node = json!({"type": "blockquote", "content": [node]});
        }
        let payload = format!("[v2]{}", serde_json::to_string(&json!([node])).unwrap());
        let rendered = tiptap_markdown(&json!(payload));
        assert!(rendered.contains("innermost"), "{rendered}");
        assert!(rendered.starts_with("> > > > > > > > > >"), "{rendered}");
    }
}
