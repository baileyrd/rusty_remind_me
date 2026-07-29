//! This node's own peer server: accepts a `push` from another node and
//! serves a `pull` request for one — the same protocol this node's own
//! sync worker uses against a configured hub, since (per the reference)
//! hub and peer are the same protocol against different endpoints. There is
//! deliberately no separate "hub mode": any node with a secret configured
//! can serve either role.
//!
//! Mirrors `webhook.rs`'s hand-rolled `TcpListener` server closely (bearer
//! auth, capped header/body reads, one connection at a time), duplicated
//! rather than shared: each hand-rolled HTTP surface in this crate is
//! self-contained, and this one additionally needs query-string parsing
//! `webhook.rs`'s single fixed-path endpoint never had to do.

use super::record::{upsert_record, SyncRecord};
use crate::Database;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub const HEALTH_PATH: &str = "/health";
pub const PUSH_PATH: &str = "/sync/push";
pub const PULL_PATH: &str = "/sync/pull";

pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Server-side cap on a pull page, regardless of what a caller asks for.
pub const MAX_PULL_LIMIT: usize = 500;
const ACCEPT_POLL: Duration = Duration::from_millis(25);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PeerServerConfig {
    pub bind: String,
    pub port: u16,
    secret: String,
    node_id: String,
}

impl std::fmt::Debug for PeerServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerServerConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("node_id", &self.node_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl PeerServerConfig {
    /// Build a configuration directly, bypassing the environment — what
    /// tests use to exercise [`serve_once`] over a fake stream without
    /// touching process-global env vars.
    pub fn new(
        bind: impl Into<String>,
        port: u16,
        secret: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            bind: bind.into(),
            port,
            secret: secret.into(),
            node_id: node_id.into(),
        }
    }

    /// `None` when no secret is configured -- a surgical check independent
    /// of the full `sync_enabled()` gate (which also requires a hub URL,
    /// irrelevant to whether *this node* should accept inbound requests).
    pub fn from_env() -> Option<Self> {
        let secret = std::env::var(super::SYNC_SECRET_ENV)
            .ok()
            .filter(|s| !s.is_empty())?;
        let bind = std::env::var(super::PEER_BIND_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| super::DEFAULT_PEER_BIND.to_string());
        let port = std::env::var(super::PEER_PORT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(super::DEFAULT_PEER_PORT);
        Some(Self {
            bind,
            port,
            secret,
            node_id: super::configured_node_id(),
        })
    }

    fn expected_authorization(&self) -> String {
        format!("Bearer {}", self.secret)
    }
}

// ---------------------------------------------------------------------------
// Request parsing (head + query string)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum ContentLength {
    Absent,
    Invalid,
    Value(usize),
}

struct Head {
    method: String,
    path: String,
    query: String,
    authorization: String,
    content_length: ContentLength,
}

enum HeadOutcome {
    Complete(Head, Vec<u8>),
    TooLarge,
    Unusable,
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_head<R: Read>(stream: &mut R) -> io::Result<HeadOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    let split = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Ok(HeadOutcome::TooLarge);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(HeadOutcome::Unusable);
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let body_prefix = buf.split_off(split + 4);
    buf.truncate(split);

    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");

    let Some(request_line) = lines.next() else {
        return Ok(HeadOutcome::Unusable);
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(HeadOutcome::Unusable);
    };
    if method.is_empty() || target.is_empty() {
        return Ok(HeadOutcome::Unusable);
    }

    let mut authorization = String::new();
    let mut content_length = ContentLength::Absent;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.to_string(),
            "content-length" => {
                content_length = match value.parse::<usize>() {
                    Ok(n) => ContentLength::Value(n),
                    Err(_) => ContentLength::Invalid,
                }
            }
            _ => {}
        }
    }

    let mut target_parts = target.splitn(2, '?');
    let path = target_parts.next().unwrap_or(target).to_string();
    let query = target_parts.next().unwrap_or("").to_string();

    Ok(HeadOutcome::Complete(
        Head {
            method: method.to_string(),
            path,
            query,
            authorization,
            content_length,
        },
        body_prefix,
    ))
}

/// Percent-decode a query string value. The inverse of `pull.rs`'s
/// `urlencode` -- tolerant of a raw (un-encoded) byte too, since this only
/// ever needs to decode what this crate's own client sent.
fn urldecode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| urldecode(v))
    })
}

fn drain_body<R: Read>(stream: &mut R, declared: &ContentLength, already_read: usize) {
    let ContentLength::Value(length) = declared else {
        return;
    };
    let mut remaining = (*length).min(MAX_BODY_BYTES).saturating_sub(already_read);
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => return,
            Ok(read) => remaining -= read,
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

fn write_response<W: Write>(stream: &mut W, status: u16, body: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason_phrase(status),
        payload.len()
    )?;
    stream.write_all(&payload)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

fn handle_health(config: &PeerServerConfig) -> Value {
    json!({ "status": "ok", "node_id": config.node_id, "time": chrono::Utc::now().to_rfc3339() })
}

fn handle_push(conn: &Connection, body: &[u8]) -> (u16, Value) {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return (400, json!({ "error": "malformed JSON" }));
    };
    let Some(records) = payload.get("records").and_then(Value::as_array) else {
        return (400, json!({ "error": "missing 'records' array" }));
    };

    let mut processed_ids = Vec::new();
    let mut failed = 0usize;
    for record_value in records {
        match serde_json::from_value::<SyncRecord>(record_value.clone()) {
            Ok(record) => match upsert_record(conn, &record) {
                Ok(_) => processed_ids.push(record.id),
                Err(_) => failed += 1,
            },
            Err(_) => failed += 1,
        }
    }

    (
        200,
        json!({ "accepted": processed_ids.len(), "processed_ids": processed_ids, "failed": failed }),
    )
}

const SYNC_RECORD_COLUMNS: &str =
    "id, content, category, tags, source, metadata, created_at, updated_at, \
     capture_id, node_id, client, accessed_at, access_count, decay_rate, vitality, base_weight, \
     status, memory_type, source_capture_id, subject, predicate, object, superseded_by";

fn parse_sync_record_row(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let tags_json: String = row.get("tags")?;
    let metadata_json: String = row.get("metadata")?;
    Ok(json!({
        "id": row.get::<_, String>("id")?,
        "content": row.get::<_, String>("content")?,
        "category": row.get::<_, String>("category")?,
        "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or_else(|_| json!([])),
        "source": row.get::<_, String>("source")?,
        "metadata": serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| json!({})),
        "created_at": row.get::<_, String>("created_at")?,
        "updated_at": row.get::<_, String>("updated_at")?,
        "capture_id": row.get::<_, Option<String>>("capture_id")?,
        "node_id": row.get::<_, Option<String>>("node_id")?,
        "client": row.get::<_, String>("client")?,
        "accessed_at": row.get::<_, Option<String>>("accessed_at")?,
        "access_count": row.get::<_, i64>("access_count")?,
        "decay_rate": row.get::<_, f64>("decay_rate")?,
        "vitality": row.get::<_, f64>("vitality")?,
        "base_weight": row.get::<_, f64>("base_weight")?,
        "status": row.get::<_, String>("status")?,
        "memory_type": row.get::<_, String>("memory_type")?,
        "source_capture_id": row.get::<_, Option<String>>("source_capture_id")?,
        "subject": row.get::<_, Option<String>>("subject")?,
        "predicate": row.get::<_, Option<String>>("predicate")?,
        "object": row.get::<_, Option<String>>("object")?,
        "superseded_by": row.get::<_, Option<String>>("superseded_by")?,
    }))
}

fn handle_pull(conn: &Connection, query: &str) -> (u16, Value) {
    let since = query_param(query, "since")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    let since_id = query_param(query, "since_id").unwrap_or_default();
    let exclude_node = query_param(query, "exclude_node").filter(|s| !s.is_empty());
    let limit: usize = query_param(query, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_PULL_LIMIT)
        .clamp(1, MAX_PULL_LIMIT);

    let sql = format!(
        "SELECT {cols} FROM memories
          WHERE (updated_at > ?1 OR (updated_at = ?1 AND id > ?2))
            {exclude_clause}
          ORDER BY updated_at ASC, id ASC
          LIMIT ?3",
        cols = SYNC_RECORD_COLUMNS,
        exclude_clause = if exclude_node.is_some() {
            "AND (node_id IS NULL OR node_id != ?4)"
        } else {
            ""
        },
    );

    let result = if let Some(exclude_node) = &exclude_node {
        conn.prepare(&sql).and_then(|mut stmt| {
            stmt.query_map(
                params![since, since_id, limit as i64, exclude_node],
                parse_sync_record_row,
            )?
            .collect::<rusqlite::Result<Vec<Value>>>()
        })
    } else {
        conn.prepare(&sql).and_then(|mut stmt| {
            stmt.query_map(
                params![since, since_id, limit as i64],
                parse_sync_record_row,
            )?
            .collect::<rusqlite::Result<Vec<Value>>>()
        })
    };

    match result {
        Ok(records) => (200, json!({ "records": records, "count": records.len() })),
        Err(e) => (500, json!({ "error": e.to_string() })),
    }
}

/// Handle one request on an already-accepted connection. Authentication
/// comes before routing, matching the webhook endpoint's own reasoning: an
/// unauthenticated caller must not be able to distinguish "wrong path" from
/// "wrong method" from "not authenticated" by comparing status codes.
pub fn serve_once<S: Read + Write>(
    stream: &mut S,
    config: &PeerServerConfig,
    conn: &Connection,
) -> io::Result<()> {
    let (head, body_prefix) = match read_head(stream)? {
        HeadOutcome::Complete(head, prefix) => (head, prefix),
        HeadOutcome::TooLarge => {
            return write_response(
                stream,
                431,
                &json!({ "error": "request headers too large" }),
            )
        }
        HeadOutcome::Unusable => return Ok(()),
    };

    if !crate::webhook::constant_time_eq(
        head.authorization.as_bytes(),
        config.expected_authorization().as_bytes(),
    ) {
        drain_body(stream, &head.content_length, body_prefix.len());
        return write_response(stream, 401, &json!({ "error": "unauthorized" }));
    }

    match (head.method.as_str(), head.path.as_str()) {
        ("GET", HEALTH_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            write_response(stream, 200, &handle_health(config))
        }
        ("GET", PULL_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            let (status, response) = handle_pull(conn, &head.query);
            write_response(stream, status, &response)
        }
        ("POST", PUSH_PATH) => {
            let length = match head.content_length {
                ContentLength::Invalid => {
                    return write_response(
                        stream,
                        400,
                        &json!({ "error": "invalid content-length" }),
                    )
                }
                ContentLength::Absent | ContentLength::Value(0) => {
                    return write_response(stream, 400, &json!({ "error": "missing request body" }))
                }
                ContentLength::Value(n) if n > MAX_BODY_BYTES => {
                    drain_body(stream, &head.content_length, body_prefix.len());
                    return write_response(
                        stream,
                        413,
                        &json!({ "error": "request body too large" }),
                    );
                }
                ContentLength::Value(n) => n,
            };
            let mut body = body_prefix;
            body.truncate(length);
            body.reserve(length.saturating_sub(body.len()));
            let mut chunk = [0u8; 8192];
            while body.len() < length {
                let want = (length - body.len()).min(chunk.len());
                match stream.read(&mut chunk[..want])? {
                    0 => break,
                    read => body.extend_from_slice(&chunk[..read]),
                }
            }
            if body.len() < length {
                return write_response(stream, 400, &json!({ "error": "truncated request body" }));
            }
            let (status, response) = handle_push(conn, &body);
            write_response(stream, status, &response)
        }
        (method, path) if path == HEALTH_PATH || path == PULL_PATH || path == PUSH_PATH => {
            drain_body(stream, &head.content_length, body_prefix.len());
            let _ = method;
            write_response(stream, 405, &json!({ "error": "method not allowed" }))
        }
        _ => {
            drain_body(stream, &head.content_length, body_prefix.len());
            write_response(stream, 404, &json!({ "error": "not found" }))
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

pub struct PeerServer {
    bind: String,
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PeerServer {
    fn start(config: PeerServerConfig, db: Arc<Database>) -> io::Result<Self> {
        let listener = TcpListener::bind((config.bind.as_str(), config.port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let bind = config.bind.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("sync-peer-server".to_string())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _peer)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                            {
                                let conn = db.conn();
                                let _ = serve_once(&mut stream, &config, &conn);
                            }
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL)
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            bind,
            port,
            shutdown,
            handle: Some(handle),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn bind(&self) -> &str {
        &self.bind
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// This node's peer server in whichever state it ended up in — mirrors
/// `Webhook` exactly, including the same shutdown-ordering reasoning (a
/// holder that also owns the `Database` must declare this field before it).
pub enum SyncPeer {
    Disabled,
    Running(PeerServer),
    Failed {
        bind: String,
        port: u16,
        error: String,
    },
}

impl SyncPeer {
    pub fn from_env(db: Arc<Database>) -> Self {
        match PeerServerConfig::from_env() {
            Some(config) => {
                let (bind, port) = (config.bind.clone(), config.port);
                match PeerServer::start(config, db) {
                    Ok(server) => Self::Running(server),
                    Err(e) => Self::Failed {
                        bind,
                        port,
                        error: e.to_string(),
                    },
                }
            }
            None => Self::Disabled,
        }
    }

    pub fn stop(&mut self) {
        if let Self::Running(server) = self {
            server.stop();
        }
    }

    pub fn status(&self) -> PeerServerStatus {
        match self {
            Self::Disabled => PeerServerStatus {
                enabled: false,
                running: false,
                bind: None,
                port: None,
                start_error: None,
                hint: Some(format!(
                    "set {} (plus {} and {} to also push/pull as a client) to accept sync requests from other nodes",
                    super::SYNC_SECRET_ENV,
                    super::NODE_ID_ENV,
                    super::HUB_URL_ENV
                )),
            },
            Self::Failed { bind, port, error } => PeerServerStatus {
                enabled: true,
                running: false,
                bind: Some(bind.clone()),
                port: Some(*port),
                start_error: Some(error.clone()),
                hint: Some(format!("configured but not listening; {bind}:{port} could not be bound")),
            },
            Self::Running(server) => PeerServerStatus {
                enabled: true,
                running: server.is_running(),
                bind: Some(server.bind.clone()),
                port: Some(server.port),
                start_error: None,
                hint: None,
            },
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerServerStatus {
    pub enabled: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}
