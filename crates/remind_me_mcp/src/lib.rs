use remind_me_core::{
    backup, capture, db::queries, entity, normalize, stats, vitality, wiki, wiki_import,
    AnnotateInput, AutoCaptureInput, Database, DecomposeBatchInput, DecomposeInput, EntityInput,
    EntityTraverseInput, ExtractBatchInput, FeedbackInput, MemoryAddInput, MemoryListInput,
    MemorySearchInput, MemoryUpdateInput, NormalizeApplyInput, NormalizeBatchInput,
    ReclassifyBatchInput, ReclassifyInput, UpdateOutcome, WikiDeleteOutcome, ANNOTATE_BATCH_MAX,
    ANNOTATE_BATCH_MIN, DECOMPOSE_BATCH_MAX, DECOMPOSE_BATCH_MIN, DECOMPOSE_FACTS_MAX,
    DECOMPOSE_FACTS_MIN, EXTRACT_BATCH_MAX, EXTRACT_BATCH_MIN, NORMALIZE_APPLY_MAX,
    NORMALIZE_APPLY_MIN, NORMALIZE_BATCH_MAX, NORMALIZE_BATCH_MIN, RECLASSIFY_BATCH_MAX,
    RECLASSIFY_BATCH_MIN,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

/// Input schema for `remind_me_annotate`.
///
/// Built here rather than inline in the `tools/list` literal: that literal is
/// one `json!` invocation covering every tool, and this schema's nesting depth
/// pushed the macro past its expansion recursion limit. Interpolating an
/// already-built `Value` costs no expansion depth. Any further deeply-nested
/// schema should be extracted the same way.
fn annotate_input_schema() -> Value {
    let entity = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "aliases": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name"]
    });

    let annotation = json!({
        "type": "object",
        "properties": {
            "memory_id": { "type": "string" },
            "subject": { "type": "string" },
            "predicate": { "type": "string" },
            "object": { "type": "string" },
            "entities": { "type": "array", "items": entity }
        },
        "required": ["memory_id"]
    });

    json!({
        "type": "object",
        "properties": {
            "annotations": {
                "type": "array",
                "minItems": ANNOTATE_BATCH_MIN,
                "maxItems": ANNOTATE_BATCH_MAX,
                "items": annotation
            }
        },
        "required": ["annotations"]
    })
}

/// Input schema for `remind_me_normalize_apply`.
///
/// Extracted for the same reason as [`annotate_input_schema`] — nesting an
/// entity array inside an entry array inside the batch pushes the shared
/// `json!` literal past its expansion recursion limit.
fn normalize_apply_input_schema() -> Value {
    let entity = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "aliases": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name"]
    });

    let entry = json!({
        "type": "object",
        "properties": {
            "memory_id": { "type": "string", "description": "The raw import being distilled" },
            "question": { "type": "string", "maxLength": 500 },
            "summary": { "type": "string", "maxLength": 10000 },
            "resolution": { "type": "string", "maxLength": 5000 },
            "refs": { "type": "array", "items": { "type": "string" }, "maxItems": 20 },
            "entities": { "type": "array", "items": entity, "maxItems": 20 }
        },
        "required": ["memory_id", "question", "summary"]
    });

    json!({
        "type": "object",
        "properties": {
            "normalizations": {
                "type": "array",
                "minItems": NORMALIZE_APPLY_MIN,
                "maxItems": NORMALIZE_APPLY_MAX,
                "items": entry
            }
        },
        "required": ["normalizations"]
    })
}

/// Input schema for `remind_me_decompose`.
///
/// Extracted for the same reason as [`annotate_input_schema`]: an entity array
/// nested inside a fact array inside the batch exceeds the shared `json!`
/// literal's expansion recursion limit.
fn decompose_input_schema() -> Value {
    let entity = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "aliases": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name"]
    });

    let fact = json!({
        "type": "object",
        "properties": {
            "content": { "type": "string", "minLength": 1, "maxLength": 50000 },
            "memory_type": { "type": "string", "description": "decision, preference, fact, insight, learning, blocker or action_item; defaults to unclassified" },
            "extra_tags": { "type": "array", "items": { "type": "string" }, "description": "Merged with the parent capture's tags" },
            "subject": { "type": "string", "maxLength": 200 },
            "predicate": { "type": "string", "maxLength": 200 },
            "object": { "type": "string", "maxLength": 500 },
            "entities": { "type": "array", "items": entity, "maxItems": 20 }
        },
        "required": ["content"]
    });

    json!({
        "type": "object",
        "properties": {
            "capture_id": { "type": "string", "minLength": 1 },
            "facts": {
                "type": "array",
                "minItems": DECOMPOSE_FACTS_MIN,
                "maxItems": DECOMPOSE_FACTS_MAX,
                "items": fact
            }
        },
        "required": ["capture_id", "facts"]
    })
}

pub struct McpServer {
    db: Database,
}

impl McpServer {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn handle_request(&self, request_json: &str) -> Option<Value> {
        let req: Value = serde_json::from_str(request_json).ok()?;
        let method = req.get("method")?.as_str()?;
        let id = req.get("id").cloned();

        match method {
            "initialize" => {
                let req_id = id.unwrap_or(json!(1));
                let requested_version = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("2024-11-05");

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": requested_version,
                        "capabilities": {
                            "tools": {
                                "listChanged": true
                            },
                            "resources": {
                                "listChanged": true,
                                "subscribe": true
                            },
                            "prompts": {
                                "listChanged": true
                            },
                            "logging": {}
                        },
                        "serverInfo": {
                            "name": "rusty_remind_me",
                            "version": "0.1.0"
                        }
                    }
                }))
            }
            "notifications/initialized" => None,
            "notifications/cancelled" => None,
            "ping" => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {}
                }))
            }
            "tools/list" => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "tools": [
                            {
                                "name": "remind_me_add",
                                "description": "Store a new memory fact, preference, or note.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "content": { "type": "string" },
                                        "category": { "type": "string", "default": "general" },
                                        "tags": { "type": "array", "items": { "type": "string" } },
                                        "source": { "type": "string", "default": "manual" }
                                    },
                                    "required": ["content"]
                                }
                            },
                            {
                                "name": "remind_me_get",
                                "description": "Get a memory by ID.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string" }
                                    },
                                    "required": ["id"]
                                }
                            },
                            {
                                "name": "remind_me_list",
                                "description": "List memories with optional filtering by category, tags, or source. Results are paginated, newest first.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "category": { "type": "string" },
                                        "tags": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Memory must have ALL of these tags"
                                        },
                                        "source": { "type": "string" },
                                        "limit": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 },
                                        "offset": { "type": "integer", "default": 0, "minimum": 0 }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_update",
                                "description": "Update an existing memory's content, category, tags, or metadata.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" },
                                        "content": { "type": "string" },
                                        "category": { "type": "string" },
                                        "tags": { "type": "array", "items": { "type": "string" } },
                                        "metadata": { "type": "object" }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_delete",
                                "description": "Delete a memory by ID.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_search",
                                "description": "Search memories using FTS5 keyword & hybrid ranking. The three expansion flags surface adjacent memories in their own sections, outside the ranked results, so they never consume `limit`.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": { "type": "string" },
                                        "limit": { "type": "integer", "default": 20 },
                                        "category": { "type": "string" },
                                        "include_dormant": { "type": "boolean", "default": false, "description": "Include memories that have decayed below the vitality floor" },
                                        "min_vitality": { "type": "number", "default": 0, "description": "Only return memories at or above this current vitality" },
                                        "expand_entities": { "type": "boolean", "default": false, "description": "Also surface memories mentioning the same entities" },
                                        "include_neighbors": { "type": "boolean", "default": false, "description": "Also surface adjacent chunks of the same source document" },
                                        "expand_co_retrieval": { "type": "boolean", "default": false, "description": "Also surface memories frequently retrieved alongside these" }
                                    },
                                    "required": ["query"]
                                }
                            },
                            {
                                "name": "remind_me_entity",
                                "description": "Upsert or fetch knowledge graph entity.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "kind": { "type": "string" }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_decompose",
                                "description": "Break a captured conversation into individually searchable atomic facts, each linked to the capture. A fact whose subject/predicate match an existing fact but whose object differs supersedes it.",
                                "inputSchema": decompose_input_schema()
                            },
                            {
                                "name": "remind_me_decompose_batch",
                                "description": "Fetch captures that have not been decomposed into atomic facts yet.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": DECOMPOSE_BATCH_MIN, "maximum": DECOMPOSE_BATCH_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_extract_batch",
                                "description": "Fetch memories that have no structured triple and no entity mentions yet, so they can be annotated with remind_me_annotate. Raw captured dialogs are excluded — their facts come out through remind_me_decompose instead.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": EXTRACT_BATCH_MIN, "maximum": EXTRACT_BATCH_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_auto_capture",
                                "description": "Capture a whole conversation as two linked memories: the verbatim dialog and a concise summary. They share a capture_id, which remind_me_get_capture retrieves both by and remind_me_decompose breaks into atomic facts.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "conversation": { "type": "string", "minLength": 1, "maxLength": 500000, "description": "The verbatim exchange" },
                                        "summary": { "type": "string", "minLength": 1, "maxLength": 50000, "description": "A concise distillation of it" },
                                        "title": { "type": "string", "maxLength": 200, "description": "Defaults to the summary's first line" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 20 },
                                        "category": { "type": "string", "default": "conversation", "maxLength": 100, "description": "Category for the SUMMARY; the dialog is always stored as 'dialog'" },
                                        "metadata": { "type": "object" }
                                    },
                                    "required": ["conversation", "summary"]
                                }
                            },
                            {
                                "name": "remind_me_get_capture",
                                "description": "Retrieve a linked dialog and summary pair by their shared capture_id.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "capture_id": { "type": "string" }
                                    },
                                    "required": ["capture_id"]
                                }
                            },
                            {
                                "name": "remind_me_normalize_batch",
                                "description": "Fetch raw imported memories (document/chat imports) that have not been normalized yet, so they can be distilled into a {question, summary, resolution?} shape. The raw memory is kept, not replaced.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": NORMALIZE_BATCH_MIN, "maximum": NORMALIZE_BATCH_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_normalize_apply",
                                "description": "Write distilled normalizations back as new memories, each linked to the raw import it came from via a normalized_from metadata pointer. The raw memory is left untouched.",
                                "inputSchema": normalize_apply_input_schema()
                            },
                            {
                                "name": "remind_me_entity_traverse",
                                "description": "Multi-hop traversal of the typed entity-relation graph from a starting entity. Unlike remind_me_entity (one entity's direct facts) this follows entity_relations edges in both directions, so it answers questions that chain relations rather than co-mention.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string", "description": "Entity name or alias to start from (case/whitespace-insensitive)", "minLength": 1, "maxLength": 200 },
                                        "hops": { "type": "integer", "default": 1, "minimum": 1, "maximum": 3, "description": "Maximum traversal depth. 1 = direct relations only." },
                                        "relation": { "type": "string", "maxLength": 200, "description": "Optional: only follow edges whose relation label matches exactly" },
                                        "cap": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100, "description": "Max number of relation edges to return" }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_write",
                                "description": "Write or update a markdown wiki page.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "slug": { "type": "string" },
                                        "title": { "type": "string" },
                                        "content": { "type": "string" },
                                        "summary": { "type": "string", "description": "One-line summary shown in the wiki index" }
                                    },
                                    "required": ["slug", "title", "content"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_read",
                                "description": "Read a wiki page by slug.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "slug": { "type": "string" }
                                    },
                                    "required": ["slug"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_import",
                                "description": "Import a directory of Markdown files into the wiki. Each file's slug/title/topic come from its YAML front matter, falling back to its first '# ' heading then its filename. Idempotent - pages upsert on slug. Pairs with `dbs export-wiki --out-dir` from daily-backup-system.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "dir": { "type": "string" },
                                        "recursive": { "type": "boolean", "default": true }
                                    },
                                    "required": ["dir"]
                                }
                            },
                            {
                                "name": "remind_me_reclassify",
                                "description": "Apply memory_type classifications. Updates each memory's decay rate to match its new type.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "classifications": {
                                            "type": "array",
                                            "minItems": 1,
                                            "maxItems": 100,
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "memory_id": { "type": "string" },
                                                    "memory_type": { "type": "string" }
                                                },
                                                "required": ["memory_id", "memory_type"]
                                            }
                                        }
                                    },
                                    "required": ["classifications"]
                                }
                            },
                            {
                                "name": "remind_me_reclassify_batch",
                                "description": "Fetch a batch of still-unclassified memories, with content snippets, to classify and feed back through remind_me_reclassify.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_feedback",
                                "description": "Mark a memory helpful or unhelpful. Without `query` this is a global judgement that adjusts the memory's weight; with `query` it is recorded as feedback for similar future searches only.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" },
                                        "signal": { "type": "string", "enum": ["helpful", "unhelpful"] },
                                        "query": { "type": "string", "maxLength": 500 }
                                    },
                                    "required": ["memory_id", "signal"]
                                }
                            },
                            {
                                "name": "remind_me_backup",
                                "description": "Create a WAL-safe online backup of the memory database, beside the database file. Older backups beyond the retention count are pruned.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {}
                                }
                            },
                            {
                                "name": "remind_me_annotate",
                                "description": "Apply subject/predicate/object triples and entity mentions to existing memories, in batches of up to 100.",
                                "inputSchema": annotate_input_schema()
                            },
                            {
                                "name": "remind_me_vitality_report",
                                "description": "Vault health report: active/dormant counts, average vitality, distribution buckets, and a breakdown by category.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": {
                                            "type": "string",
                                            "enum": ["json", "markdown"],
                                            "default": "json"
                                        }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_wiki_search",
                                "description": "Full-text search wiki page titles and content, ranked by BM25.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": { "type": "string", "minLength": 1, "maxLength": 500 },
                                        "limit": { "type": "integer", "default": 10, "minimum": 1, "maximum": 50 }
                                    },
                                    "required": ["query"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_list",
                                "description": "List every wiki page, most recently updated first.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {}
                                }
                            },
                            {
                                "name": "remind_me_wiki_delete",
                                "description": "Delete a wiki page by title or slug.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "title": {
                                            "type": "string",
                                            "description": "Page title or slug",
                                            "minLength": 1,
                                            "maxLength": 200
                                        }
                                    },
                                    "required": ["title"]
                                }
                            },
                            {
                                "name": "remind_me_stats",
                                "description": "Get database stats and memory counts.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {}
                                }
                            }
                        ]
                    }
                }))
            }
            "resources/list" => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "resources": [
                            {
                                "uri": "memory://stats",
                                "name": "Memory Engine Statistics",
                                "mimeType": "application/json"
                            }
                        ]
                    }
                }))
            }
            "resources/read" => {
                let req_id = id.unwrap_or(json!(1));
                let conn = self.db.conn();
                match stats::collect(&conn) {
                    Ok(s) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "contents": [
                                {
                                    "uri": "memory://stats",
                                    "mimeType": "application/json",
                                    "text": serde_json::to_string_pretty(&s).unwrap()
                                }
                            ]
                        }
                    })),
                    Err(e) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": -32603, "message": format!("Stats error: {}", e) }
                    })),
                }
            }
            "prompts/list" => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "prompts": [
                            {
                                "name": "recall_context",
                                "description": "Recall long-term memory facts relevant to the ongoing conversation topic.",
                                "arguments": [
                                    { "name": "topic", "description": "Topic or entity keyword to search", "required": true }
                                ]
                            }
                        ]
                    }
                }))
            }
            "tools/call" => {
                let req_id = id.unwrap_or(json!(1));
                let params = req.get("params")?;
                let tool_name = params.get("name")?.as_str()?;
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let conn = self.db.conn();

                let result = match tool_name {
                    "remind_me_add" => {
                        let input: Result<MemoryAddInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(add_input) => match queries::add_memory(&conn, add_input) {
                                Ok(mem) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&mem).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Database error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_get" => {
                        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        match queries::get_memory_by_id(&conn, id) {
                            Ok(Some(mem)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&mem).unwrap() }] })
                            }
                            Ok(None) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": "Memory not found" }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_list" => {
                        let input: Result<MemoryListInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(list_input) => match queries::list_memories(&conn, &list_input) {
                                Ok(page) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&page).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("List error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid list input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_update" => {
                        let input: Result<MemoryUpdateInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(update_input) => {
                                match queries::update_memory(&conn, &update_input) {
                                    Ok(UpdateOutcome::Updated(mem)) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&mem).unwrap() }] })
                                    }
                                    Ok(UpdateOutcome::NotFound) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Memory `{}` not found", update_input.memory_id) }] })
                                    }
                                    Ok(UpdateOutcome::NoFields) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": "Nothing to update — no fields provided" }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Update error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid update input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_delete" => {
                        let memory_id =
                            args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
                        match queries::delete_memory(&conn, memory_id) {
                            Ok(true) => {
                                json!({ "content": [{ "type": "text", "text": format!("Memory `{}` deleted", memory_id) }] })
                            }
                            Ok(false) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Memory `{}` not found", memory_id) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Delete error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_search" => {
                        let input: Result<MemorySearchInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(search_input) => {
                                match queries::search_with_expansions(&conn, &search_input) {
                                    Ok(res) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&res).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Search error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid search input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_entity" => {
                        let input: Result<EntityInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(ent_input) => match entity::upsert_entity(&conn, &ent_input) {
                                Ok(ent) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&ent).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Entity error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid entity input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_extract_batch" => {
                        let input: ExtractBatchInput = serde_json::from_value(args)
                            .unwrap_or(ExtractBatchInput { batch_size: 20 });
                        match queries::unannotated_batch(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Extract batch error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_decompose" => {
                        let input: Result<DecomposeInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(decompose_input) => {
                                if decompose_input.facts.len() < DECOMPOSE_FACTS_MIN
                                    || decompose_input.facts.len() > DECOMPOSE_FACTS_MAX
                                {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("facts must hold {}..={} entries", DECOMPOSE_FACTS_MIN, DECOMPOSE_FACTS_MAX) }] })
                                } else {
                                    match capture::decompose(&conn, &decompose_input) {
                                        Ok(Some(result)) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                        }
                                        Ok(None) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("No memory found with capture_id {:?}.", decompose_input.capture_id) }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Decompose error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid decompose input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_decompose_batch" => {
                        let input: DecomposeBatchInput = serde_json::from_value(args)
                            .unwrap_or(DecomposeBatchInput { batch_size: 20 });
                        match capture::undecomposed_batch(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Decompose batch error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_auto_capture" => {
                        let input: Result<AutoCaptureInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(capture_input) => {
                                match capture::auto_capture(&conn, &capture_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Auto capture error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid auto capture input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_get_capture" => {
                        let capture_id = args
                            .get("capture_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match capture::get_capture(&conn, capture_id) {
                            Ok(Some(found)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&found).unwrap() }] })
                            }
                            Ok(None) => {
                                json!({ "content": [{ "type": "text", "text": format!("No capture found with id {:?}.", capture_id) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Get capture error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_normalize_batch" => {
                        let input: NormalizeBatchInput = serde_json::from_value(args)
                            .unwrap_or(NormalizeBatchInput { batch_size: 20 });
                        match normalize::unnormalized_batch(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Normalize batch error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_normalize_apply" => {
                        let input: Result<NormalizeApplyInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(apply_input) => {
                                if apply_input.normalizations.len() < NORMALIZE_APPLY_MIN
                                    || apply_input.normalizations.len() > NORMALIZE_APPLY_MAX
                                {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("normalizations must hold {}..={} entries", NORMALIZE_APPLY_MIN, NORMALIZE_APPLY_MAX) }] })
                                } else {
                                    match normalize::apply_normalizations(&conn, &apply_input) {
                                        Ok(result) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Normalize apply error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid normalize apply input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_entity_traverse" => {
                        let input: Result<EntityTraverseInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(traverse_input) => {
                                match entity::traverse_from_name(&conn, &traverse_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Entity traverse error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid entity traverse input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_write" => {
                        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                        match wiki::write_wiki_page(&conn, slug, title, content, summary) {
                            Ok(page) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&page).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki write error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_read" => {
                        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                        match wiki::get_wiki_page(&conn, slug) {
                            Ok(Some(page)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&page).unwrap() }] })
                            }
                            Ok(None) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": "Wiki page not found" }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki read error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_import" => {
                        let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or("");
                        let recursive = args
                            .get("recursive")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        match wiki_import::import_wiki_dir(
                            &conn,
                            std::path::Path::new(dir),
                            recursive,
                        ) {
                            Ok(report) => {
                                let imported: Vec<_> = report.imported.iter().map(|p| json!({
                                    "slug": p.slug, "title": p.title, "topic": p.topic, "path": p.path
                                })).collect();
                                let skipped: Vec<_> = report
                                    .skipped
                                    .iter()
                                    .map(|(path, reason)| {
                                        json!({
                                            "path": path, "reason": reason
                                        })
                                    })
                                    .collect();
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&json!({
                                    "imported_count": imported.len(),
                                    "skipped_count": skipped.len(),
                                    "imported": imported,
                                    "skipped": skipped
                                })).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki import error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_reclassify" => {
                        let input: Result<ReclassifyInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(rc) => {
                                let count = rc.classifications.len();
                                if !(RECLASSIFY_BATCH_MIN..=RECLASSIFY_BATCH_MAX).contains(&count) {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("`classifications` must hold {}..={} items, got {}", RECLASSIFY_BATCH_MIN, RECLASSIFY_BATCH_MAX, count) }] })
                                } else {
                                    match queries::reclassify_memories(&conn, &rc) {
                                        Ok(outcome) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reclassify error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid reclassify input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_reclassify_batch" => {
                        let input: Result<ReclassifyBatchInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(batch) => match queries::unclassified_batch(&conn, &batch) {
                                Ok(result) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reclassify batch error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid batch input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_feedback" => {
                        let input: Result<FeedbackInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(fb) => match vitality::record_feedback(
                                &conn,
                                &fb.memory_id,
                                fb.signal,
                                fb.query.as_deref(),
                            ) {
                                Ok(Some(v)) => {
                                    let body = json!({
                                        "memory_id": fb.memory_id,
                                        "signal": fb.signal,
                                        "vitality": v
                                    });
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                                }
                                Ok(None) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Memory `{}` not found", fb.memory_id) }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Feedback error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid feedback input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_backup" => match backup::create_backup(&conn, "manual") {
                        Ok(outcome) => {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Backup error: {}", e) }] })
                        }
                    },
                    "remind_me_annotate" => {
                        let input: Result<AnnotateInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(annotate_input) => {
                                let count = annotate_input.annotations.len();
                                if !(ANNOTATE_BATCH_MIN..=ANNOTATE_BATCH_MAX).contains(&count) {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("`annotations` must hold {}..={} items, got {}", ANNOTATE_BATCH_MIN, ANNOTATE_BATCH_MAX, count) }] })
                                } else {
                                    match queries::annotate_memories(&conn, &annotate_input) {
                                        Ok(outcome) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Annotate error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid annotate input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_vitality_report" => match vitality::build_vitality_report(&conn) {
                        Ok(report) => {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Vitality report error: {}", e) }] })
                        }
                    },
                    "remind_me_wiki_search" => {
                        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(wiki::WIKI_SEARCH_LIMIT_DEFAULT);
                        if query.is_empty() {
                            json!({ "isError": true, "content": [{ "type": "text", "text": "`query` is required" }] })
                        } else {
                            match wiki::search_wiki_pages(&conn, query, limit) {
                                Ok(hits) => {
                                    let body = json!({ "count": hits.len(), "results": hits });
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki search error: {}", e) }] })
                                }
                            }
                        }
                    }
                    "remind_me_wiki_list" => match wiki::list_wiki_pages(&conn) {
                        Ok(pages) => {
                            let body = json!({ "count": pages.len(), "pages": pages });
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki list error: {}", e) }] })
                        }
                    },
                    "remind_me_wiki_delete" => {
                        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        if title.is_empty() {
                            json!({ "isError": true, "content": [{ "type": "text", "text": "`title` is required" }] })
                        } else {
                            match wiki::delete_wiki_page(&conn, title) {
                                Ok(WikiDeleteOutcome::Deleted) => {
                                    json!({ "content": [{ "type": "text", "text": format!("Wiki page '{}' deleted", title) }] })
                                }
                                Ok(WikiDeleteOutcome::NotFound) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki page '{}' not found", title) }] })
                                }
                                Ok(WikiDeleteOutcome::Reserved) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("'{}' is a reserved system page and cannot be deleted", title) }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki delete error: {}", e) }] })
                                }
                            }
                        }
                    }
                    "remind_me_stats" => match stats::collect(&conn) {
                        Ok(s) => {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&s).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Stats error: {}", e) }] })
                        }
                    },
                    _ => {
                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Unknown tool: {}", tool_name) }] })
                    }
                };

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": result
                }))
            }
            _ => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": "Method not found"
                    }
                }))
            }
        }
    }

    pub fn run_stdio_loop(&self) -> io::Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let handle = stdin.lock();

        for line in handle.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(resp) = self.handle_request(&line) {
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(stdout, "{}", resp_str)?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_initialize_dynamic_version() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28"
            }
        });
        let resp = server.handle_request(&req.to_string()).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2026-07-28");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], true);
    }

    fn call(server: &McpServer, name: &str, args: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        server.handle_request(&req.to_string()).unwrap()["result"].clone()
    }

    fn text_of(result: &Value) -> String {
        result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn test_crud_tools_are_registered() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        for expected in ["remind_me_list", "remind_me_update", "remind_me_delete"] {
            assert!(names.contains(&expected), "{} not in tools/list", expected);
        }
    }

    #[test]
    fn test_crud_tools_round_trip_over_jsonrpc() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(
            &server,
            "remind_me_add",
            json!({ "content": "a fact worth keeping" }),
        );
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let listed = call(&server, "remind_me_list", json!({}));
        let page: Value = serde_json::from_str(&text_of(&listed)).unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["memories"][0]["id"], id);

        let updated = call(
            &server,
            "remind_me_update",
            json!({ "memory_id": id, "content": "a revised fact" }),
        );
        assert!(
            updated.get("isError").is_none(),
            "update failed: {:?}",
            updated
        );
        assert!(text_of(&updated).contains("a revised fact"));

        let deleted = call(&server, "remind_me_delete", json!({ "memory_id": id }));
        assert!(
            deleted.get("isError").is_none(),
            "delete failed: {:?}",
            deleted
        );

        let after = call(&server, "remind_me_list", json!({}));
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&after)).unwrap()["total"],
            0
        );
    }

    #[test]
    fn test_crud_tools_surface_errors() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let missing = call(
            &server,
            "remind_me_delete",
            json!({ "memory_id": "mem_nope" }),
        );
        assert_eq!(missing["isError"], true);

        let no_fields = call(
            &server,
            "remind_me_update",
            json!({ "memory_id": "mem_nope" }),
        );
        assert_eq!(no_fields["isError"], true);

        // `limit` is typed usize; a negative value must be an input error rather
        // than a silent clamp to zero.
        let bad_limit = call(&server, "remind_me_list", json!({ "limit": -3 }));
        assert_eq!(bad_limit["isError"], true);
    }

    #[test]
    fn test_feedback_tool_both_modes() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(&server, "remind_me_add", json!({ "content": "a fact" }));
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Global: no query, so the weight moves and the response reports it.
        let global = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": id, "signal": "helpful" }),
        );
        assert!(global.get("isError").is_none(), "failed: {:?}", global);
        let body: Value = serde_json::from_str(&text_of(&global)).unwrap();
        assert_eq!(body["signal"], "helpful");
        assert!(body["vitality"].as_f64().unwrap() > 1.0);

        // Contextual: a query is supplied, so vitality is reported unchanged.
        let contextual = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": id, "signal": "unhelpful", "query": "some question" }),
        );
        let after: Value = serde_json::from_str(&text_of(&contextual)).unwrap();
        assert_eq!(after["vitality"], body["vitality"]);
    }

    #[test]
    fn test_feedback_rejects_a_bad_signal_and_unknown_memory() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let bad = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": "mem_x", "signal": "maybe" }),
        );
        assert_eq!(bad["isError"], true, "signal is a closed set of two values");

        let missing = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": "mem_nope", "signal": "helpful" }),
        );
        assert_eq!(missing["isError"], true);
    }

    #[test]
    fn test_backup_tool_is_registered_and_takes_no_parameters() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_backup")
            .expect("remind_me_backup not in tools/list");

        assert!(
            tool["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .is_empty(),
            "backup takes no caller-supplied destination"
        );
    }

    #[test]
    fn test_backup_of_an_in_memory_database_reports_why() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let result = call(&server, "remind_me_backup", json!({}));
        assert_eq!(result["isError"], true);
        assert!(
            text_of(&result).contains("in memory"),
            "expected an explanation, got: {}",
            text_of(&result)
        );
    }

    #[test]
    fn test_annotate_tool_round_trip_and_partial_failure() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(&server, "remind_me_add", json!({ "content": "a fact" }));
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let annotated = call(
            &server,
            "remind_me_annotate",
            json!({ "annotations": [
                { "memory_id": id, "predicate": "uses", "entities": [{ "name": "SQLite" }] },
                { "memory_id": "mem_nope", "predicate": "uses" }
            ]}),
        );
        assert!(annotated.get("isError").is_none());

        let body: Value = serde_json::from_str(&text_of(&annotated)).unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["errors"].as_array().unwrap().len(), 1);
        assert_eq!(body["results"][0]["entities_linked"], 1);

        let fetched = call(&server, "remind_me_get", json!({ "id": id }));
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&fetched)).unwrap()["predicate"],
            "uses"
        );
    }

    #[test]
    fn test_annotate_rejects_an_out_of_range_batch() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let empty = call(&server, "remind_me_annotate", json!({ "annotations": [] }));
        assert_eq!(empty["isError"], true);

        let oversized: Vec<Value> = (0..ANNOTATE_BATCH_MAX + 1)
            .map(|_| json!({ "memory_id": "mem_x" }))
            .collect();
        let too_many = call(
            &server,
            "remind_me_annotate",
            json!({ "annotations": oversized }),
        );
        assert_eq!(too_many["isError"], true);
    }

    #[test]
    fn test_vitality_report_tool() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_vitality_report")
            .expect("remind_me_vitality_report not in tools/list");
        assert_eq!(
            tool["inputSchema"]["properties"]["response_format"]["default"], "json",
            "the reference defaults this tool to JSON, unlike most others"
        );

        call(&server, "remind_me_add", json!({ "content": "a memory" }));
        let report = call(&server, "remind_me_vitality_report", json!({}));
        assert!(
            report.get("isError").is_none(),
            "report failed: {:?}",
            report
        );

        let body: Value = serde_json::from_str(&text_of(&report)).unwrap();
        assert_eq!(body["total_memories"], 1);
        assert_eq!(body["vault_health_score"], "100%");
        assert!(body["vitality_buckets"].is_object());
    }

    #[test]
    fn test_wiki_tools_are_registered() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        for expected in ["remind_me_wiki_list", "remind_me_wiki_delete"] {
            assert!(names.contains(&expected), "{} not in tools/list", expected);
        }
    }

    #[test]
    fn test_wiki_tools_round_trip_over_jsonrpc() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        call(
            &server,
            "remind_me_wiki_write",
            json!({ "slug": "vlan-setup", "title": "VLAN Setup", "content": "body" }),
        );

        let listed = call(&server, "remind_me_wiki_list", json!({}));
        let page: Value = serde_json::from_str(&text_of(&listed)).unwrap();
        assert_eq!(page["count"], 1);
        assert_eq!(page["pages"][0]["slug"], "vlan-setup");

        // Address it by human title rather than slug.
        let deleted = call(
            &server,
            "remind_me_wiki_delete",
            json!({ "title": "VLAN Setup" }),
        );
        assert!(
            deleted.get("isError").is_none(),
            "delete failed: {:?}",
            deleted
        );

        let after = call(&server, "remind_me_wiki_list", json!({}));
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&after)).unwrap()["count"],
            0
        );
    }

    #[test]
    fn test_wiki_search_tool() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        assert!(
            resp["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "remind_me_wiki_search"),
            "remind_me_wiki_search not in tools/list"
        );

        call(
            &server,
            "remind_me_wiki_write",
            json!({ "slug": "vlan", "title": "VLAN Setup", "content": "trunking notes",
                    "summary": "how to trunk" }),
        );

        let hits = call(
            &server,
            "remind_me_wiki_search",
            json!({ "query": "trunking" }),
        );
        assert!(hits.get("isError").is_none(), "search failed: {:?}", hits);
        let body: Value = serde_json::from_str(&text_of(&hits)).unwrap();
        assert_eq!(body["count"], 1);
        assert_eq!(body["results"][0]["slug"], "vlan");
        assert_eq!(body["results"][0]["summary"], "how to trunk");

        let missing = call(&server, "remind_me_wiki_search", json!({}));
        assert_eq!(missing["isError"], true);
    }

    #[test]
    fn test_memory_search_survives_punctuation() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        call(
            &server,
            "remind_me_add",
            json!({ "content": "the plan is to ship" }),
        );

        // Unsanitised, this is an FTS5 syntax error rather than a search.
        let found = call(
            &server,
            "remind_me_search",
            json!({ "query": "what's the plan, exactly?" }),
        );
        assert!(
            found.get("isError").is_none(),
            "search errored: {:?}",
            found
        );
        assert!(text_of(&found).contains("the plan is to ship"));
    }

    #[test]
    fn test_wiki_delete_surfaces_errors() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let missing = call(&server, "remind_me_wiki_delete", json!({ "title": "nope" }));
        assert_eq!(missing["isError"], true);

        let blank = call(&server, "remind_me_wiki_delete", json!({ "title": "" }));
        assert_eq!(blank["isError"], true);

        call(
            &server,
            "remind_me_wiki_write",
            json!({ "slug": "index", "title": "Index", "content": "body" }),
        );
        let reserved = call(
            &server,
            "remind_me_wiki_delete",
            json!({ "title": "index" }),
        );
        assert_eq!(reserved["isError"], true);
        assert!(text_of(&reserved).contains("reserved"));
    }

    #[test]
    fn test_mcp_resources_and_prompts() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let res_req = json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" });
        let res_resp = server.handle_request(&res_req.to_string()).unwrap();
        assert_eq!(res_resp["result"]["resources"][0]["uri"], "memory://stats");

        let prompt_req = json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/list" });
        let prompt_resp = server.handle_request(&prompt_req.to_string()).unwrap();
        assert_eq!(
            prompt_resp["result"]["prompts"][0]["name"],
            "recall_context"
        );
    }
}
