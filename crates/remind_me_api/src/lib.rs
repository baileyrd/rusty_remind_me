use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct ApiServer {
    db: Arc<Database>,
}

impl ApiServer {
    pub fn new(db: Database) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn status(&self) -> Value {
        json!({
            "status": "ok",
            "server": "rusty_remind_me_api",
            "version": "0.1.0"
        })
    }

    pub async fn run(&self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr).await?;
        println!("REST API server listening on http://{}", addr);

        loop {
            let (mut socket, _) = listener.accept().await?;
            let db = Arc::clone(&self.db);

            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                let bytes_read = match socket.read(&mut buffer).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };

                let request_str = String::from_utf8_lossy(&buffer[..bytes_read]);
                let mut lines = request_str.lines();
                let request_line = match lines.next() {
                    Some(line) => line,
                    None => return,
                };

                let parts: Vec<&str> = request_line.split_whitespace().collect();
                if parts.len() < 2 {
                    return;
                }

                let method = parts[0];
                let path = parts[1];

                let body = request_str.split("\r\n\r\n").nth(1).unwrap_or("").trim();

                let (status_code, response_body) = match (method, path) {
                    ("GET", "/health") => (200, json!({ "status": "ok", "version": "0.1.0" })),
                    ("GET", "/stats") => {
                        let conn = db.conn();
                        let count: i64 = conn
                            .query_row(
                                "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
                                [],
                                |r| r.get(0),
                            )
                            .unwrap_or(0);
                        (200, json!({ "total_memories": count }))
                    }
                    ("POST", "/api/v1/memories") => {
                        match serde_json::from_str::<MemoryAddInput>(body) {
                            Ok(input) => {
                                let conn = db.conn();
                                match queries::add_memory(&conn, input) {
                                    Ok(mem) => (201, json!(mem)),
                                    Err(e) => (500, json!({ "error": e.to_string() })),
                                }
                            }
                            Err(e) => (400, json!({ "error": format!("Invalid JSON: {}", e) })),
                        }
                    }
                    ("POST", "/api/v1/search") => {
                        match serde_json::from_str::<MemorySearchInput>(body) {
                            Ok(input) => {
                                let conn = db.conn();
                                match queries::search_memories(&conn, &input) {
                                    Ok(results) => (200, json!(results)),
                                    Err(e) => (500, json!({ "error": e.to_string() })),
                                }
                            }
                            Err(e) => (400, json!({ "error": format!("Invalid JSON: {}", e) })),
                        }
                    }
                    _ => (404, json!({ "error": "Not Found" })),
                };

                let response_json = response_body.to_string();
                let http_response = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_code,
                    response_json.len(),
                    response_json
                );

                let _ = socket.write_all(http_response.as_bytes()).await;
            });
        }
    }
}
