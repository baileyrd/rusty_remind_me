use remind_me_core::{
    db::queries, entity, wiki, wiki_import, Database, EntityInput, MemoryAddInput, MemoryListInput,
    MemorySearchInput, MemoryUpdateInput, UpdateOutcome,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

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
                                "description": "Search memories using FTS5 keyword & hybrid ranking.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": { "type": "string" },
                                        "limit": { "type": "integer", "default": 20 }
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
                                "name": "remind_me_wiki_write",
                                "description": "Write or update a markdown wiki page.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "slug": { "type": "string" },
                                        "title": { "type": "string" },
                                        "content": { "type": "string" },
                                        "topic": { "type": "string" }
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
                let count: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "contents": [
                            {
                                "uri": "memory://stats",
                                "mimeType": "application/json",
                                "text": serde_json::to_string_pretty(&json!({ "total_memories": count })).unwrap()
                            }
                        ]
                    }
                }))
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
                                match queries::search_memories(&conn, &search_input) {
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
                    "remind_me_wiki_write" => {
                        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let topic = args
                            .get("topic")
                            .and_then(|v| v.as_str())
                            .unwrap_or("general");
                        match wiki::write_wiki_page(&conn, slug, title, content, topic) {
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
                    "remind_me_stats" => {
                        let count: i64 = conn
                            .query_row(
                                "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
                                [],
                                |r| r.get(0),
                            )
                            .unwrap_or(0);
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&json!({ "total_memories": count })).unwrap() }] })
                    }
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
