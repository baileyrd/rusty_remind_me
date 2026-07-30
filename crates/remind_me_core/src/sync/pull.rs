//! Pull a remote's changes into this node.
//!
//! Mirrors the reference's `_pull_remote`: a keyset cursor `(updated_at,
//! id)` persisted in `sync_log`, paging `GET /sync/pull` until a short page
//! confirms the remote is drained, with a hard cap on pages per cycle and
//! an early stop if a page makes no cursor progress (a misbehaving remote
//! that keeps replaying the same page cannot trap a cycle forever).

use super::graph::{
    upsert_entity_record, upsert_entity_relation_record, upsert_link_record,
    EntityRelationSyncRecord, EntitySyncRecord, LinkSyncRecord,
};
use super::http;
use super::record::{canon_ts, upsert_record, SyncRecord};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

pub const PULL_PAGE_SIZE: usize = 500;
/// Safety valve so a misbehaving or very large remote cannot keep one pull
/// cycle running indefinitely -- matches the reference's own cap.
pub const MAX_PULL_PAGES: usize = 100;

const EPOCH: &str = "1970-01-01T00:00:00+00:00";

#[derive(Debug)]
pub struct PullError(pub String);

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for PullError {}
impl From<rusqlite::Error> for PullError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<http::HttpError> for PullError {
    fn from(e: http::HttpError) -> Self {
        Self(e.to_string())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PullReport {
    pub applied: usize,
    pub failed: usize,
    pub pages: usize,
}

/// Percent-encode a query parameter value. Hand-rolled rather than a new
/// dependency: every value this module actually sends is an RFC3339
/// timestamp, an id, or an operator-configured node id, so a conservative
/// unreserved-characters allowlist is all this needs.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn read_cursor(conn: &Connection, remote_id: &str) -> rusqlite::Result<(String, String)> {
    conn.query_row(
        "SELECT last_pull, last_pull_id FROM sync_log WHERE remote_id = ?",
        params![remote_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map(|opt| opt.unwrap_or_else(|| (EPOCH.to_string(), String::new())))
}

fn persist_cursor(
    conn: &Connection,
    remote_id: &str,
    since: &str,
    since_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_log (remote_id, last_pull, last_pull_id) VALUES (?, ?, ?)
         ON CONFLICT(remote_id) DO UPDATE SET last_pull = excluded.last_pull, last_pull_id = excluded.last_pull_id",
        params![remote_id, since, since_id],
    )?;
    Ok(())
}

/// Pull every change `remote_id` (reached at `hub_url`) has made since the
/// last cursor, applying each via [`upsert_record`]. `node_id` is this
/// node's own id, sent as `exclude_node` so a hub does not hand this node
/// back the very rows it originated.
pub fn pull_remote(
    conn: &Connection,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    remote_id: &str,
) -> Result<PullReport, PullError> {
    let (mut since, mut since_id) = read_cursor(conn, remote_id)?;
    let mut report = PullReport::default();

    for _ in 0..MAX_PULL_PAGES {
        let url = format!(
            "{}/sync/pull?since={}&since_id={}&exclude_node={}&limit={}",
            hub_url.trim_end_matches('/'),
            urlencode(&since),
            urlencode(&since_id),
            urlencode(node_id),
            PULL_PAGE_SIZE,
        );
        let (status, body) = http::get(&url, secret)?;
        if !(200..300).contains(&status) {
            return Err(PullError(format!(
                "pull from {} returned {}: {}",
                url,
                status,
                body.trim()
            )));
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| PullError(format!("pull response from {} was not JSON: {}", url, e)))?;
        let records = parsed
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if records.is_empty() {
            break;
        }
        report.pages += 1;

        let mut page_max = (since.clone(), since_id.clone());
        for record_value in &records {
            match serde_json::from_value::<SyncRecord>(record_value.clone()) {
                Ok(record) => {
                    let record_ts = canon_ts(&record.updated_at);
                    match upsert_record(conn, &record) {
                        Ok(_outcome) => {
                            report.applied += 1;
                            if (record_ts.clone(), record.id.clone()) > page_max {
                                page_max = (record_ts, record.id.clone());
                            }
                        }
                        Err(_) => report.failed += 1,
                    }
                }
                Err(_) => report.failed += 1,
            }
        }

        let records_len = records.len();
        if page_max <= (since.clone(), since_id.clone()) {
            // No progress: applying this page didn't advance the cursor at
            // all (every record failed, or the remote keeps replaying the
            // same page). Stop rather than loop forever on it.
            break;
        }
        since = page_max.0;
        since_id = page_max.1;
        persist_cursor(conn, remote_id, &since, &since_id)?;

        if records_len < PULL_PAGE_SIZE {
            break;
        }
    }

    Ok(report)
}

/// Shared paging loop for the three graph-table pull endpoints
/// (`/sync/pull_entities`, `/sync/pull_links`, `/sync/pull_entity_relations`)
/// — same keyset-cursor/no-progress/short-page mechanics as [`pull_remote`],
/// parameterized by endpoint, the cursor's own `sync_log` key (namespaced
/// `"{remote_id}#entities"` etc., matching the reference exactly, since one
/// remote has up to four independent cursors: memories plus these three),
/// and which JSON fields a raw record uses for cursor advancement.
///
/// A `404` from the endpoint is tolerated silently (an older peer that
/// predates graph sync) — matching the reference's own tolerance — and
/// deliberately does not write a cursor, so a peer that's upgraded later
/// gets pulled from scratch rather than from a cursor position that was
/// never real.
#[allow(clippy::too_many_arguments)]
fn pull_graph_table(
    conn: &Connection,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    cursor_key: &str,
    endpoint: &str,
    ts_field: &str,
    id_field: &str,
    apply: impl Fn(&Connection, &Value) -> Result<(), String>,
) -> Result<PullReport, PullError> {
    let (mut since, mut since_id) = read_cursor(conn, cursor_key)?;
    let mut report = PullReport::default();

    for _ in 0..MAX_PULL_PAGES {
        let url = format!(
            "{}/{}?since={}&since_id={}&exclude_node={}&limit={}",
            hub_url.trim_end_matches('/'),
            endpoint,
            urlencode(&since),
            urlencode(&since_id),
            urlencode(node_id),
            PULL_PAGE_SIZE,
        );
        let (status, body) = http::get(&url, secret)?;
        if status == 404 {
            // An older peer that predates this endpoint -- not an error.
            break;
        }
        if !(200..300).contains(&status) {
            return Err(PullError(format!(
                "pull from {} returned {}: {}",
                url,
                status,
                body.trim()
            )));
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| PullError(format!("pull response from {} was not JSON: {}", url, e)))?;
        let records = parsed
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if records.is_empty() {
            break;
        }
        report.pages += 1;

        let mut page_max = (since.clone(), since_id.clone());
        for record_value in &records {
            let cursor_fields = record_value
                .get(ts_field)
                .and_then(Value::as_str)
                .map(canon_ts)
                .zip(
                    record_value
                        .get(id_field)
                        .and_then(Value::as_str)
                        .map(String::from),
                );
            match apply(conn, record_value) {
                Ok(()) => {
                    report.applied += 1;
                    if let Some(candidate) = cursor_fields {
                        if candidate > page_max {
                            page_max = candidate;
                        }
                    }
                }
                Err(_) => report.failed += 1,
            }
        }

        let records_len = records.len();
        if page_max <= (since.clone(), since_id.clone()) {
            break;
        }
        since = page_max.0;
        since_id = page_max.1;
        persist_cursor(conn, cursor_key, &since, &since_id)?;

        if records_len < PULL_PAGE_SIZE {
            break;
        }
    }

    Ok(report)
}

/// Pull `entities` changes, keyset-paged on `(updated_at, id)` like
/// `memories`.
pub fn pull_entities(
    conn: &Connection,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    remote_id: &str,
) -> Result<PullReport, PullError> {
    let cursor_key = format!("{remote_id}#entities");
    pull_graph_table(
        conn,
        hub_url,
        secret,
        node_id,
        &cursor_key,
        "sync/pull_entities",
        "updated_at",
        "id",
        |c, v| {
            let record: EntitySyncRecord =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            upsert_entity_record(c, &record).map_err(|e| e.to_string())
        },
    )
}

/// Pull `memory_entities` mention links, keyset-paged on `(created_at,
/// memory_id||'|'||entity_id)` — links have no `updated_at` (immutable) and
/// no real single-column id, so the cursor uses the same synthetic
/// composite key the wire `id` field already carries.
pub fn pull_links(
    conn: &Connection,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    remote_id: &str,
) -> Result<PullReport, PullError> {
    let cursor_key = format!("{remote_id}#links");
    pull_graph_table(
        conn,
        hub_url,
        secret,
        node_id,
        &cursor_key,
        "sync/pull_links",
        "created_at",
        "id",
        |c, v| {
            let record: LinkSyncRecord =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            upsert_link_record(c, &record)
                .map(|_| ())
                .map_err(|e| e.to_string())
        },
    )
}

/// Pull `entity_relations`, keyset-paged on `(created_at, id)` — relations
/// already have a real deterministic id, so no synthetic key is needed.
pub fn pull_entity_relations(
    conn: &Connection,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    remote_id: &str,
) -> Result<PullReport, PullError> {
    let cursor_key = format!("{remote_id}#entity_relations");
    pull_graph_table(
        conn,
        hub_url,
        secret,
        node_id,
        &cursor_key,
        "sync/pull_entity_relations",
        "updated_at",
        "id",
        |c, v| {
            let record: EntityRelationSyncRecord =
                serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
            upsert_entity_relation_record(c, &record).map_err(|e| e.to_string())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_leaves_unreserved_characters_alone() {
        assert_eq!(urlencode("node-1_2.3~4"), "node-1_2.3~4");
    }

    #[test]
    fn urlencode_percent_encodes_a_timestamp() {
        assert_eq!(
            urlencode("2026-01-01T00:00:00+00:00"),
            "2026-01-01T00%3A00%3A00%2B00%3A00"
        );
    }
}
