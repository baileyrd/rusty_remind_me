// The `tools/list` response is one `json!` literal covering every tool, and
// `json!` expands recursively — so the limit is reached by the *number* of
// tools as well as by any one schema's nesting depth. Extracting a schema into
// its own function (see `annotate_input_schema`) fixes the depth case; nothing
// but a higher limit fixes the breadth case short of building the array as a
// `Vec<Value>` from several smaller literals. Raise this when it bites again,
// or do that restructuring — the literal is around thirty tools now, and the
// restructuring is the better answer once it is closer to sixty.
#![recursion_limit = "512"]

use remind_me_core::{
    backup, capture,
    consolidation::consolidate,
    db::queries,
    dbs_import, entity, export, importer, mempalace_import, normalize, stats, status,
    sync::{SyncPeer, SyncWorker},
    updater, vectors, vitality, watcher,
    webhook::Webhook,
    wiki,
    wiki_fs::Wiki,
    wiki_import, AnnotateInput, AutoCaptureInput, BulkImportDirInput, ChatImportInput,
    ConsolidateInput, Database, DbsImportInput, DecomposeBatchInput, DecomposeInput, EntityInput,
    EntityTraverseInput, ExportInput, ExtractBatchInput, FeedbackInput, MemoryAddInput,
    MemoryListInput, MemorySearchInput, MemoryUpdateInput, MempalaceImportInput,
    NormalizeApplyInput, NormalizeBatchInput, ReclassifyBatchInput, ReclassifyInput, UpdateOutcome,
    WikiDeleteOutcome, ANNOTATE_BATCH_MAX, ANNOTATE_BATCH_MIN, CONSOLIDATE_LIMIT_MAX,
    CONSOLIDATE_LIMIT_MIN, CONSOLIDATE_SIMILARITY_MAX, CONSOLIDATE_SIMILARITY_MIN,
    DBS_IMPORT_LIMIT_MAX, DBS_IMPORT_LIMIT_MIN, DECOMPOSE_BATCH_MAX, DECOMPOSE_BATCH_MIN,
    DECOMPOSE_FACTS_MAX, DECOMPOSE_FACTS_MIN, EXTRACT_BATCH_MAX, EXTRACT_BATCH_MIN, EXTRACT_MODES,
    IMPORT_MAX_LENGTH_MAX, IMPORT_MAX_LENGTH_MIN, MEMPALACE_IMPORT_LIMIT_MAX,
    MEMPALACE_IMPORT_LIMIT_MIN, NORMALIZE_APPLY_MAX, NORMALIZE_APPLY_MIN, NORMALIZE_BATCH_MAX,
    NORMALIZE_BATCH_MIN, RECLASSIFY_BATCH_MAX, RECLASSIFY_BATCH_MIN,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

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

/// Input schema shared by the two import tools.
///
/// `directory` takes a folder and a `recursive` flag; `file_path` takes one
/// file. Everything else is identical, so the shape is built once.
fn import_input_schema(directory: bool) -> Value {
    let mut properties = json!({
        "category": { "type": "string", "default": "chat_import", "description": "Category for imported memories. A document import replaces the chat default with 'document'." },
        "tags": { "type": "array", "items": { "type": "string" } },
        "extract_mode": {
            "type": "string",
            "enum": EXTRACT_MODES,
            "default": "assistant_messages",
            "description": "Which turns to keep from a chat export"
        },
        "max_length": { "type": "integer", "default": 10000, "minimum": IMPORT_MAX_LENGTH_MIN, "maximum": IMPORT_MAX_LENGTH_MAX, "description": "Characters per memory; longer content is chunked" },
        "kind": { "type": "string", "enum": ["auto", "chat", "document"], "default": "auto" }
    });
    let object = properties.as_object_mut().expect("just built an object");
    if directory {
        object.insert("directory".into(), json!({ "type": "string" }));
        object.insert(
            "recursive".into(),
            json!({ "type": "boolean", "default": true, "description": "Search subdirectories" }),
        );
    } else {
        object.insert("file_path".into(), json!({ "type": "string" }));
    }

    json!({
        "type": "object",
        "properties": properties,
        "required": [if directory { "directory" } else { "file_path" }]
    })
}

pub struct McpServer {
    /// Declared **before** `db`, and that ordering is load-bearing: Rust drops
    /// struct fields in declaration order, `Webhook`'s drop joins its serving
    /// thread, and that thread writes through the database. Listed after `db`
    /// it would still be sound — the thread holds its own `Arc` — but the
    /// connections would stay open until the thread noticed, which is the
    /// shutdown ordering `SE-07` exists to pin down. `sync_peer` and
    /// `sync_worker` hold their own serving/background threads for the same
    /// reason and are declared before `db` for the same reason.
    webhook: Webhook,
    sync_peer: SyncPeer,
    sync_worker: Option<SyncWorker>,
    db: Arc<Database>,
    wiki: Wiki,
}

impl McpServer {
    pub fn new(db: Database) -> Self {
        Self::with_wiki(db, Wiki::from_env())
    }

    /// Build a server against a specific wiki directory.
    ///
    /// Tests need this: the default root is a real shared directory, so a test
    /// using it would write into whatever wiki the machine's user actually has.
    pub fn with_wiki(db: Database, wiki: Wiki) -> Self {
        let db = Arc::new(db);
        Self {
            // A no-op without `REMIND_ME_WEBHOOK_SECRET`, which is the ordinary
            // case — nothing binds a port unless someone asked for one.
            webhook: Webhook::from_env(Arc::clone(&db)),
            // A no-op without `REMIND_ME_SYNC_SECRET` — accepting another
            // node's push/pull is off by default exactly like the webhook.
            sync_peer: SyncPeer::from_env(Arc::clone(&db)),
            // `None` unless node id, hub URL, and secret are all configured —
            // matching the reference's own `SYNC_ENABLED` gate exactly.
            sync_worker: SyncWorker::from_env(Arc::clone(&db)),
            db,
            wiki,
        }
    }

    /// Stop the push endpoint without tearing the server down.
    pub fn stop_webhook(&mut self) {
        self.webhook.stop();
    }

    /// The push endpoint's state, as `remind_me_webhook_status` reports it.
    pub fn webhook_status(&self) -> remind_me_core::webhook::WebhookStatus {
        self.webhook.status()
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
                                "name": "remind_me_import_chat",
                                "description": "Import a chat export or document file into memory. .json/.jsonl are chat exports; .md/.markdown/.txt are content-sniffed in auto mode — chat role markers import as chat, everything else as a document chunked per section. Deduplicates by file content hash. The path must be inside the allowed import roots.",
                                "inputSchema": import_input_schema(false)
                            },
                            {
                                "name": "remind_me_import_directory",
                                "description": "Import every supported file in a directory. Same parsing and per-file dedup as remind_me_import_chat.",
                                "inputSchema": import_input_schema(true)
                            },
                            {
                                "name": "remind_me_import_dbs",
                                "description": "Bulk-import a daily-backup-system (dbs) archive. Reads its items/sources tables directly, read-only, and turns each live item into a memory with dbs's source and tags preserved as knowledge-graph entities rather than flattened into prose. Reruns are safe: unchanged items are skipped, and an item edited since its last import gets a fresh memory that supersedes the old one. Page a large archive with offset until has_more is false.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "db_path": { "type": "string", "description": "Path to the dbs SQLite archive, inside the allowed import roots" },
                                        "source": { "type": "string", "description": "Restrict to one dbs source name (e.g. 'raindrop'). Omit for all." },
                                        "item_type": { "type": "string", "description": "Restrict to one dbs item_kind (e.g. 'link'). Omit for all." },
                                        "limit": {
                                            "type": "integer",
                                            "minimum": DBS_IMPORT_LIMIT_MIN,
                                            "maximum": DBS_IMPORT_LIMIT_MAX,
                                            "default": 500
                                        },
                                        "offset": { "type": "integer", "minimum": 0, "default": 0 },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Extra tags added to every imported memory" },
                                        "dry_run": { "type": "boolean", "default": false, "description": "Report what would be imported without writing" }
                                    },
                                    "required": ["db_path"]
                                }
                            },
                            {
                                "name": "remind_me_import_mempalace",
                                "description": "Bulk-import memories from a MemPalace ChromaDB store, one page at a time. Reads its metadata segment directly, read-only, rather than one drawer at a time via MemPalace's own tools. A drawer carrying remind_me's own memory frontmatter has its category/tags/created restored; everything else is stored as one opaque memory per drawer, tagged with its wing and room. Already-imported drawers are skipped (tracked by drawer id), so reruns are safe. Reads REMIND_ME_MEMPALACE_PATH for the store location.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "wing": { "type": "string", "description": "Restrict to one wing (project). Omit for all." },
                                        "room": { "type": "string", "description": "Restrict to one room within the wing. Omit for all." },
                                        "limit": {
                                            "type": "integer",
                                            "minimum": MEMPALACE_IMPORT_LIMIT_MIN,
                                            "maximum": MEMPALACE_IMPORT_LIMIT_MAX,
                                            "default": 500
                                        },
                                        "offset": { "type": "integer", "minimum": 0, "default": 0 },
                                        "category": { "type": "string", "description": "Category for a drawer with no restorable frontmatter category (default: mempalace_import)" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Extra tags added to every imported memory" },
                                        "dry_run": { "type": "boolean", "default": false, "description": "Report what would be imported without writing" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_watch_status",
                                "description": "Report the folder watcher: which directories are watched, which were refused for sitting outside the import roots, scan counts, and recent errors. Says what to configure when nothing is.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_check_update",
                                "description": "Check whether this checkout is behind origin/main. Read-only: fetches from the remote and compares commits, never modifies anything.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_self_update",
                                "description": "Pull the latest changes from origin/main (fast-forward only) and rebuild the workspace in release mode. Refuses a working tree with uncommitted changes unless force is set; force never bypasses the fast-forward-only pull, so a diverged local history is still refused either way. Always requires a restart to take effect on success.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "force": { "type": "boolean", "default": false, "description": "Skip the uncommitted-changes guard. Does not bypass the fast-forward-only pull." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_revoke_clients",
                                "description": "List OAuth clients registered with the remote connector's authorization server (FT-07), or revoke one by client_id. Without client_id, lists every registered client with its live access/refresh token counts -- this is the read path, not a bulk-revoke shorthand: there is no 'revoke all' operation, only 'list' (empty client_id) and 'revoke this one client' (client_id set). With client_id, deletes that client's registration and every token it holds; the live remote server re-reads the state file on each token check, so the client is locked out immediately and must re-register and re-obtain the owner's consent to reconnect.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "client_id": { "type": "string", "default": "", "description": "The client to revoke. Empty (default) lists clients instead of revoking anything." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_reindex",
                                "description": "Rebuild vector embeddings for every memory that doesn't have one yet. Existing embeddings are preserved; only missing ones are generated. Run this after configuring REMIND_ME_EMBEDDING_BACKEND, or after a bulk import that ran before an embedder was available. Reports 'degraded' when no embedder is configured or reachable, rather than silently doing nothing.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_webhook_status",
                                "description": "Report the push ingestion endpoint: whether a secret is configured, whether it is listening, where, and how many pushes it has ingested, skipped or refused. Distinguishes 'nobody configured one' from 'configured but the port could not be bound'.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_list_connectors",
                                "description": "List every registered import connector — the pluggable parsers behind remind_me_import_chat's `kind` parameter.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_server_status",
                                "description": "Report where the data lives and what is running: database path and size, schema version against what this build expects, memory count, backup inventory, and which subsystems are active. Subsystems this crate does not implement are named with a reason rather than reported as stopped.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_export_memories",
                                "description": "Export memories to JSON or JSONL as a complete logical backup — every column, plus the entity graph. Embedding vectors are excluded as derived data. Writes to file_path when given (must be inside the allowed export roots), otherwise returns the payload inline.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "format": { "type": "string", "enum": ["json", "jsonl"], "default": "json" },
                                        "category": { "type": "string", "description": "Filter: only export memories with this category" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Filter: memory must have ALL of these tags" },
                                        "file_path": { "type": "string", "description": "Destination file, inside the allowed export roots. Omit to return inline." },
                                        "include_graph": { "type": "boolean", "default": true, "description": "Append entities, links and relations as record_type-tagged records" }
                                    }
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
                                        "title": { "type": "string", "minLength": 1, "maxLength": 200, "description": "The title's slug is the page's identity — keep titles stable so [[wikilinks]] resolve" },
                                        "content": { "type": "string", "minLength": 1, "maxLength": 100000, "description": "Full markdown body, REPLACING any existing content. Open with a one-sentence summary; it becomes the index entry. A leading '# Title' is added if absent." },
                                        "log_note": { "type": "string", "maxLength": 500, "description": "Optional note recorded in log.md alongside the change" }
                                    },
                                    "required": ["title", "content"]
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
                                "name": "remind_me_wiki_load",
                                "description": "Load the whole wiki into context as one markdown document, newest-revised first up to a token budget. Overflow is listed by title so it can be read individually.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "token_budget": { "type": "integer", "default": 0, "minimum": 0, "maximum": 200000, "description": "0 means unlimited" },
                                        "include_index": { "type": "boolean", "default": true }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_wiki_compile",
                                "description": "Drive wiki synthesis over raw memories. With mark_integrated=false (the default) returns a brief of pending sources and never advances the watermark, so it is safe to call repeatedly. Call again with mark_integrated=true after writing the pages.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "limit": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 },
                                        "mark_integrated": { "type": "boolean", "default": false }
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
                                "name": "remind_me_consolidate",
                                "description": "Find clusters of near-duplicate memories by embedding similarity and optionally merge them into one canonical representative. dry_run (default true) reports clusters — canonical, members, and each member's similarity to the canonical — without changing anything. To actually merge, review the report, write a short summary per cluster you want consolidated, then call again with dry_run=false and summaries={canonical_id: summary}; a cluster with no matching summary is skipped, not merged with a raw concatenation.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "similarity_threshold": { "type": "number", "default": 0.85, "minimum": CONSOLIDATE_SIMILARITY_MIN, "maximum": CONSOLIDATE_SIMILARITY_MAX, "description": "Minimum cosine similarity to cluster memories together. Higher = stricter." },
                                        "dry_run": { "type": "boolean", "default": true, "description": "If true, report clusters without modifying data. Set false to auto-merge." },
                                        "category": { "type": "string", "description": "Limit consolidation to this category" },
                                        "limit": { "type": "integer", "default": 500, "minimum": CONSOLIDATE_LIMIT_MIN, "maximum": CONSOLIDATE_LIMIT_MAX, "description": "Maximum memories to consider (prevents runaway on large vaults)" },
                                        "summaries": { "type": "object", "additionalProperties": { "type": "string" }, "description": "{canonical_id: summary}, one entry per cluster (from a prior dry_run=true call) you want consolidated. Required to actually merge a cluster when dry_run=false." }
                                    }
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
                let mut span = remind_me_core::telemetry::maybe_span(&format!("tool.{tool_name}"));

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
                    "remind_me_import_chat" => {
                        let input: Result<ChatImportInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => match importer::import_chat(&conn, &import_input) {
                                Ok(outcome) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Import error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_import_directory" => {
                        let input: Result<BulkImportDirInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => {
                                match importer::import_directory(&conn, &import_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Directory import error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid directory import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_watch_status" => {
                        let report = match watcher::Watcher::from_env() {
                            Some(w) => w.status(),
                            None => watcher::disabled_status(),
                        };
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                    }
                    "remind_me_check_update" => {
                        let status = updater::check_for_update();
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&status).unwrap() }] })
                    }
                    "remind_me_self_update" => {
                        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                        let result = updater::perform_update(force);
                        if result.success {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                        } else {
                            json!({ "isError": true, "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                        }
                    }
                    "remind_me_revoke_clients" => {
                        let client_id = args.get("client_id").and_then(Value::as_str).unwrap_or("");
                        let store = remind_me_core::remote::OAuthStateStore::new(
                            remind_me_core::remote::oauth_state_file_path(),
                        );
                        let body = if client_id.is_empty() {
                            json!({
                                "clients": store.list_clients(),
                                "state_file": store.path().to_string_lossy(),
                                "hint": "Pass client_id to revoke a client and all of its tokens.",
                            })
                        } else {
                            match store.revoke_client(client_id) {
                                Some(summary) => {
                                    let mut body =
                                        serde_json::to_value(summary).unwrap_or(json!({}));
                                    body["status"] = json!("revoked");
                                    body
                                }
                                None => json!({
                                    "status": "error",
                                    "error": format!("Unknown client_id: {}", client_id),
                                }),
                            }
                        };
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                    }
                    "remind_me_reindex" => match vectors::reindex(&conn) {
                        Ok(result) => {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reindex error: {}", e) }] })
                        }
                    },
                    "remind_me_import_dbs" => {
                        let input: Result<DbsImportInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => match dbs_import::pull_dbs(&conn, &import_input) {
                                Ok(result) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("dbs import error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid dbs import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_import_mempalace" => {
                        let input: Result<MempalaceImportInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => {
                                match mempalace_import::pull_mempalace(&conn, &import_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("MemPalace import error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid MemPalace import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_webhook_status" => {
                        let report = self.webhook.status();
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                    }
                    "remind_me_list_connectors" => {
                        let body = json!({
                            "connectors": importer::connectors(),
                            "file_import_kinds": ["chat", "document"],
                        });
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                    }
                    "remind_me_server_status" => match status::server_status(&conn) {
                        Ok(report) => {
                            // The webhook's/sync peer's/sync worker's state
                            // lives on these structs, not on the connection,
                            // so they are merged in here rather than
                            // gathered by `server_status`.
                            let mut report = serde_json::to_value(&report).unwrap_or(json!({}));
                            report["webhook"] =
                                serde_json::to_value(self.webhook.status()).unwrap_or(json!({}));
                            report["sync_peer"] =
                                serde_json::to_value(self.sync_peer.status()).unwrap_or(json!({}));
                            report["sync"] = serde_json::to_value(
                                self.sync_worker
                                    .as_ref()
                                    .map(|w| w.status())
                                    .unwrap_or_else(
                                        remind_me_core::sync::sync_worker_disabled_status,
                                    ),
                            )
                            .unwrap_or(json!({}));
                            // The remote MCP connector (FT-05, #85) has no
                            // running state to merge in the way the webhook/
                            // sync do -- remind_me_remote::run() only exists
                            // when the CLI process opts in, and this crate
                            // stays synchronous, so there is no live server
                            // handle here to ask. remind_me_core::remote::remote_status
                            // reports config/token state the same way the
                            // reference's get_remote_status() does, purely
                            // from env vars and the token file.
                            report["remote"] =
                                serde_json::to_value(remind_me_core::remote::remote_status())
                                    .unwrap_or(json!({}));
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Server status error: {}", e) }] })
                        }
                    },
                    "remind_me_export_memories" => {
                        let input: ExportInput = serde_json::from_value(args).unwrap_or_default();
                        match export::export_memories(&conn, &input) {
                            Ok(result) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Export error: {}", e) }] })
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
                        let log_note = args.get("log_note").and_then(|v| v.as_str());
                        // The slug is derived from the title now that files are
                        // canonical: a caller-supplied slug disagreeing with the
                        // title would name a file the index could not find. A
                        // `slug` argument is still tolerated as the title when
                        // none is given, so an older caller is not broken.
                        let title = if title.is_empty() { slug } else { title };
                        match self.wiki.write_page(&conn, title, content, log_note) {
                            Ok(Ok(outcome)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                            }
                            Ok(Err(_)) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("'{}' is a reserved system page and cannot be written directly", title) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki write error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_read" => {
                        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                        match self.wiki.read_page(&conn, slug) {
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
                    "remind_me_wiki_load" => {
                        let token_budget = args
                            .get("token_budget")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(0);
                        let include_index = args
                            .get("include_index")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        match self.wiki.load(&conn, token_budget, include_index) {
                            Ok(loaded) if loaded.pages_included == 0 => {
                                json!({ "content": [{ "type": "text", "text": "_The wiki is empty._ Synthesise pages from raw memories with `remind_me_wiki_compile`." }] })
                            }
                            Ok(loaded) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&loaded).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki load error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_compile" => {
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(20)
                            .clamp(1, 100);
                        let mark_integrated = args
                            .get("mark_integrated")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        match self.wiki.compile(&conn, limit, mark_integrated) {
                            Ok(outcome) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki compile error: {}", e) }] })
                            }
                        }
                    }
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
                            match self.wiki.search_pages(&conn, query, limit) {
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
                    "remind_me_wiki_list" => match self.wiki.list_pages(&conn) {
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
                            match self.wiki.delete_page(&conn, title) {
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
                    "remind_me_consolidate" => {
                        let input: Result<ConsolidateInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(consolidate_input) => match consolidate(&conn, &consolidate_input) {
                                Ok(report) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Consolidate error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid consolidate input: {}", e) }] })
                            }
                        }
                    }
                    _ => {
                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Unknown tool: {}", tool_name) }] })
                    }
                };
                if result
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    span.mark_error();
                }

                let mut result = result;
                // Surfaces once, on whatever tool call happens to be first
                // after startup, then clears — matching the reference's own
                // one-shot startup notice, attached centrally here rather
                // than duplicated per handler since this dispatch has a
                // single point every tool call already passes through.
                if let Some(notice) = updater::pop_update_notice() {
                    if let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut())
                    {
                        content.push(json!({ "type": "text", "text": notice }));
                    }
                }

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
    fn test_consolidate_tool_is_registered_and_defaults_to_a_safe_dry_run() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_consolidate")
            .expect("remind_me_consolidate not in tools/list");
        let schema = &tool["inputSchema"]["properties"];
        assert_eq!(schema["dry_run"]["default"], true);
        assert_eq!(schema["similarity_threshold"]["default"], 0.85);
        assert_eq!(schema["similarity_threshold"]["minimum"], 0.5);
        assert_eq!(schema["similarity_threshold"]["maximum"], 1.0);
        assert_eq!(schema["limit"]["default"], 500);
        assert_eq!(schema["limit"]["minimum"], 10);
        assert_eq!(schema["limit"]["maximum"], 5000);

        // No embedder is configured in this test harness, so nothing has a
        // chunk-0 vector -- the round trip still exercises real dispatch and
        // must report cleanly rather than erroring.
        call(&server, "remind_me_add", json!({ "content": "a memory" }));
        let result = call(&server, "remind_me_consolidate", json!({}));
        assert!(result.get("isError").is_none(), "call failed: {:?}", result);

        let body: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(body["clusters_found"], 0);
        assert_eq!(body["message"], "No eligible memories found");
    }

    /// A server whose wiki lives in its own scratch directory.
    ///
    /// `McpServer::new` reads the configured wiki root, which is a real shared
    /// directory — a test using it would write into the machine user's actual
    /// wiki.
    fn wiki_server(name: &str) -> (McpServer, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("rrm_mcp_wiki_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let db = Database::open_in_memory().unwrap();
        (
            McpServer::with_wiki(db, remind_me_core::wiki_fs::Wiki::new(&root)),
            root,
        )
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
        let (server, root) = wiki_server("roundtrip");

        call(
            &server,
            "remind_me_wiki_write",
            json!({ "title": "VLAN Setup", "content": "body" }),
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

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_wiki_search_tool() {
        let (server, root) = wiki_server("search");

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
            json!({ "title": "VLAN Setup", "content": "trunking notes" }),
        );

        let hits = call(
            &server,
            "remind_me_wiki_search",
            json!({ "query": "trunking" }),
        );
        assert!(hits.get("isError").is_none(), "search failed: {:?}", hits);
        let body: Value = serde_json::from_str(&text_of(&hits)).unwrap();
        assert_eq!(body["count"], 1);
        // The slug is derived from the title, and the summary from the first
        // body line — neither is caller-supplied any more, because a file-backed
        // page has to be self-describing.
        assert_eq!(body["results"][0]["slug"], "vlan-setup");
        assert_eq!(body["results"][0]["summary"], "trunking notes");

        let missing = call(&server, "remind_me_wiki_search", json!({}));
        assert_eq!(missing["isError"], true);

        std::fs::remove_dir_all(&root).unwrap();
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
        let (server, root) = wiki_server("deleteerrors");

        let missing = call(&server, "remind_me_wiki_delete", json!({ "title": "nope" }));
        assert_eq!(missing["isError"], true);

        let blank = call(&server, "remind_me_wiki_delete", json!({ "title": "" }));
        assert_eq!(blank["isError"], true);

        // Writing a reserved page is refused too, not just deleting one.
        let written = call(
            &server,
            "remind_me_wiki_write",
            json!({ "title": "Index", "content": "body" }),
        );
        assert_eq!(written["isError"], true);
        assert!(text_of(&written).contains("reserved"));

        let reserved = call(
            &server,
            "remind_me_wiki_delete",
            json!({ "title": "index" }),
        );
        assert_eq!(reserved["isError"], true);
        assert!(text_of(&reserved).contains("reserved"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_import_dbs_is_registered_and_refuses_a_path_outside_the_roots() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 12, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_import_dbs")
            .expect("remind_me_import_dbs not in tools/list");
        assert_eq!(tool["inputSchema"]["required"][0], "db_path");
        assert_eq!(tool["inputSchema"]["properties"]["limit"]["maximum"], 2000);

        let result = call(
            &server,
            "remind_me_import_dbs",
            json!({ "db_path": "/etc/hosts" }),
        );

        assert_eq!(result["isError"], true);
        // Refused for containment, not for existence — the message must not
        // reveal whether a path outside the roots is there.
        assert!(
            text_of(&result).contains("not in allowed import roots"),
            "got {}",
            text_of(&result)
        );
    }

    #[test]
    fn test_import_mempalace_is_registered_and_has_no_path_field() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 13, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_import_mempalace")
            .expect("remind_me_import_mempalace not in tools/list");
        // Unlike remind_me_import_dbs, the store location is operator
        // configuration (REMIND_ME_MEMPALACE_PATH), not a per-call argument —
        // there is nothing required at all.
        assert!(tool["inputSchema"]["required"]
            .as_array()
            .is_none_or(|r| r.is_empty()));
        assert_eq!(tool["inputSchema"]["properties"]["limit"]["maximum"], 2000);

        // No store configured in the test environment, so this must fail —
        // as "no store found", not silently succeed with zero results, which
        // would be indistinguishable from an empty palace.
        let result = call(&server, "remind_me_import_mempalace", json!({}));
        assert_eq!(result["isError"], true);
        assert!(
            text_of(&result).contains("No MemPalace store found"),
            "got {}",
            text_of(&result)
        );
    }

    #[test]
    fn test_webhook_status_is_registered_and_reports_disabled() {
        // No `REMIND_ME_WEBHOOK_SECRET` in the test environment, so nothing
        // binds a port — which is the state this asserts.
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 11, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"remind_me_webhook_status"),
            "remind_me_webhook_status not in tools/list"
        );

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_webhook_status",
            json!({}),
        )))
        .unwrap();
        assert_eq!(report["enabled"], false);
        assert_eq!(report["running"], false);
        // Disabled, not broken: no bind failure to report, and a hint saying
        // what would turn it on.
        assert!(report["start_error"].is_null());
        assert!(report["hint"].as_str().unwrap().contains("SECRET"));
    }

    #[test]
    fn test_check_update_and_self_update_are_registered() {
        // Not invoked here, unlike every other tool this file tests end to
        // end: both discover their repository from the process's current
        // working directory (docs/adr/0003-self-update-strategy.md), and
        // remind_me_self_update would really run `git pull`/`cargo build
        // --release --workspace` against whatever repo contains this test
        // binary's cwd -- which, inside this workspace's own test suite, is
        // this very checkout. That behavior is exercised instead in
        // remind_me_core's updater.rs unit tests, against real but
        // disposable git repos under a temp directory, never this sandbox's
        // actual checkout.
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 15, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();

        let check = tools
            .iter()
            .find(|t| t["name"] == "remind_me_check_update")
            .expect("remind_me_check_update not in tools/list");
        assert!(check["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .is_empty());

        let self_update = tools
            .iter()
            .find(|t| t["name"] == "remind_me_self_update")
            .expect("remind_me_self_update not in tools/list");
        assert_eq!(
            self_update["inputSchema"]["properties"]["force"]["default"],
            false
        );
    }

    #[test]
    fn test_revoke_clients_lists_by_default_and_revokes_one_client_by_id() {
        // No other test in this file touches REMIND_ME_REMOTE_OAUTH_STATE_FILE,
        // so (matching this file's existing convention, e.g. the reindex
        // test's EMBEDDING_BACKEND_ENV) this doesn't need a cross-test lock.
        let dir = std::env::temp_dir().join(format!(
            "rrm_mcp_revoke_clients_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("oauth.json");
        std::env::set_var(
            remind_me_core::remote::REMOTE_OAUTH_STATE_FILE_ENV,
            &state_file,
        );

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 16, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let tool = tools
            .iter()
            .find(|t| t["name"] == "remind_me_revoke_clients")
            .expect("remind_me_revoke_clients not in tools/list");
        assert_eq!(
            tool["inputSchema"]["properties"]["client_id"]["default"],
            ""
        );

        // Empty client_id is the *list* path, not "revoke every client" --
        // this is the exact semantics #86 called out as easy to get backwards.
        let empty_state: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({}),
        )))
        .unwrap();
        assert_eq!(empty_state["clients"], json!([]));
        assert!(empty_state["hint"].as_str().unwrap().contains("client_id"));

        // Register a client with tokens directly against the same state
        // file a live remote server would read, the same cross-process
        // story the reference's tool relies on.
        let store = remind_me_core::remote::OAuthStateStore::new(&state_file);
        store.put_client(
            "client-1",
            json!({ "client_name": "claude.ai", "redirect_uris": ["https://claude.ai/cb"] }),
        );
        store.put_token(
            remind_me_core::remote::TokenKind::Access,
            "access-tok",
            json!({ "client_id": "client-1" }),
        );
        store.put_token(
            remind_me_core::remote::TokenKind::Refresh,
            "refresh-tok",
            json!({ "client_id": "client-1" }),
        );

        let listed: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({}),
        )))
        .unwrap();
        assert_eq!(listed["clients"][0]["client_id"], "client-1");
        assert_eq!(listed["clients"][0]["access_tokens"], 1);
        assert_eq!(listed["clients"][0]["refresh_tokens"], 1);

        // A non-empty client_id revokes that one client and its tokens.
        let revoked: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({ "client_id": "client-1" }),
        )))
        .unwrap();
        assert_eq!(revoked["status"], "revoked");
        assert_eq!(revoked["access_tokens"], 1);
        assert_eq!(revoked["refresh_tokens"], 1);

        // The client is gone -- listing is empty again, not "still there".
        let after: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({}),
        )))
        .unwrap();
        assert_eq!(after["clients"], json!([]));

        // Revoking an unknown client_id is an error, never silently a no-op
        // success and never "revoked everything that happened to exist".
        let unknown: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({ "client_id": "no-such-client" }),
        )))
        .unwrap();
        assert_eq!(unknown["status"], "error");

        std::env::remove_var(remind_me_core::remote::REMOTE_OAUTH_STATE_FILE_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reindex_is_registered_and_reports_degraded_without_an_embedder() {
        // No REMIND_ME_EMBEDDING_BACKEND in the test environment, so this
        // must report degraded rather than silently doing nothing.
        std::env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 14, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_reindex")
            .expect("remind_me_reindex not in tools/list");
        assert!(tool["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .is_empty());

        let report: Value =
            serde_json::from_str(&text_of(&call(&server, "remind_me_reindex", json!({})))).unwrap();

        assert_eq!(report["degraded"], true);
        assert_eq!(report["embedded"], 0);
    }

    #[test]
    fn test_server_status_carries_the_webhook() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_server_status",
            json!({}),
        )))
        .unwrap();

        // Merged in by the MCP layer rather than gathered from the connection,
        // because that is where the endpoint's state lives.
        assert_eq!(report["webhook"]["enabled"], false);
        assert_eq!(report["sync_peer"]["enabled"], false);
        assert_eq!(report["sync"]["enabled"], false);
        assert_eq!(report["schema_current"], true);
        // No REMIND_ME_REMOTE_MCP in the test environment, so the remote
        // connector (FT-05, #85) reports disabled the same way the others do.
        assert_eq!(report["remote"]["enabled"], false);
        assert_eq!(report["remote"]["host"], "127.0.0.1");
    }

    #[test]
    fn test_stopping_the_webhook_is_safe_when_none_is_running() {
        let db = Database::open_in_memory().unwrap();
        let mut server = McpServer::new(db);

        server.stop_webhook();
        server.stop_webhook();

        assert!(!server.webhook_status().running);
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
