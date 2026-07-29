use crate::models::{
    Memory, MemoryAddInput, MemoryListInput, MemoryListResult, MemorySearchInput,
    MemorySearchResult, MemoryUpdateInput, RetrievalStrategy, UpdateOutcome, LIST_LIMIT_MAX,
    LIST_LIMIT_MIN,
};
use crate::retrieval::{choose_rrf_weights, rank_rrf, trim_by_token_budget};
use crate::vitality::{
    calculate_vitality, get_decay_rate, get_source_prior, get_type_prior, VITALITY_FLOOR,
};
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Result, Row};

/// Columns selected wherever a full [`Memory`] is parsed via [`parse_memory_row`].
const MEMORY_COLUMNS: &str = "id, content, category, tags, source, metadata, created_at, \
     updated_at, capture_id, subject, predicate, object, superseded_by, decay_rate, vitality, \
     access_count, last_accessed_at";

pub fn parse_memory_row(row: &Row) -> Result<Memory> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();

    let meta_json: String = row.get("metadata")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&meta_json).unwrap_or(serde_json::Value::Null);

    Ok(Memory {
        id: row.get("id")?,
        content: row.get("content")?,
        category: row.get("category")?,
        tags,
        source: row.get("source")?,
        metadata,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        capture_id: row.get("capture_id")?,
        subject: row.get("subject")?,
        predicate: row.get("predicate")?,
        object: row.get("object")?,
        superseded_by: row.get("superseded_by")?,
        decay_rate: row.get("decay_rate")?,
        vitality: row.get("vitality")?,
        access_count: row.get("access_count")?,
        last_accessed_at: row.get("last_accessed_at")?,
    })
}

pub fn add_memory(conn: &Connection, input: MemoryAddInput) -> Result<Memory> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let id = format!("mem_{}", uuid::Uuid::new_v4().simple());

    let tags_json = serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".to_string());
    let metadata_json = serde_json::to_string(&input.metadata).unwrap_or_else(|_| "{}".to_string());
    let decay_rate = get_decay_rate(&input.category);
    let type_prior = get_type_prior(&input.category);
    let source_prior = get_source_prior(&input.source);
    let base_weight = type_prior * source_prior;
    let initial_vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata, created_at, updated_at,
            subject, predicate, object, decay_rate, vitality, access_count, last_accessed_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
        params![
            id,
            input.content,
            input.category,
            tags_json,
            input.source,
            metadata_json,
            now_iso,
            now_iso,
            input.subject,
            input.predicate,
            input.object,
            decay_rate,
            initial_vitality,
            now_iso,
        ],
    )?;

    get_memory_by_id(conn, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get_memory_by_id(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE id = ? AND deleted_at IS NULL",
        MEMORY_COLUMNS
    ))?;

    let mut rows = stmt.query_map(params![id], parse_memory_row)?;
    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
}

/// Build the shared `WHERE` clause and its bindings for [`list_memories`].
///
/// Tag matching is ALL-of, one `EXISTS` per requested tag. It runs over
/// `json_each(m.tags)` rather than a normalized tag table because the target has
/// no `memory_tags` table yet. Keeping the predicate in SQL (rather than
/// filtering parsed rows in Rust) is what lets `COUNT`, `LIMIT` and `OFFSET`
/// stay correct — the reference hit exactly this pagination bug as `DATA-02`.
/// When `memory_tags` lands this can become a join without changing callers.
fn list_filters(input: &MemoryListInput) -> (String, Vec<Value>) {
    let mut conditions = vec!["m.deleted_at IS NULL".to_string()];
    let mut bindings: Vec<Value> = Vec::new();

    if let Some(category) = input.category.as_ref().filter(|c| !c.is_empty()) {
        conditions.push("m.category = ?".to_string());
        bindings.push(Value::Text(category.clone()));
    }
    if let Some(source) = input.source.as_ref().filter(|s| !s.is_empty()) {
        conditions.push("m.source = ?".to_string());
        bindings.push(Value::Text(source.clone()));
    }
    for tag in input.tags.iter().flatten() {
        conditions
            .push("EXISTS (SELECT 1 FROM json_each(m.tags) je WHERE je.value = ?)".to_string());
        bindings.push(Value::Text(tag.clone()));
    }

    (format!("WHERE {}", conditions.join(" AND ")), bindings)
}

/// List memories newest-first, filtered by category, source and/or tags.
///
/// `limit` is clamped to [`LIST_LIMIT_MIN`]..=[`LIST_LIMIT_MAX`]; the clamped
/// value is echoed back in the result so callers can tell what was applied.
pub fn list_memories(conn: &Connection, input: &MemoryListInput) -> Result<MemoryListResult> {
    let limit = input.limit.clamp(LIST_LIMIT_MIN, LIST_LIMIT_MAX);
    let (where_clause, bindings) = list_filters(input);

    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memories m {}", where_clause),
        params_from_iter(bindings.iter()),
        |row| row.get(0),
    )?;

    let sql = format!(
        "SELECT {} FROM memories m {} ORDER BY m.created_at DESC, m.id DESC LIMIT ? OFFSET ?",
        MEMORY_COLUMNS, where_clause
    );
    let mut page_bindings = bindings;
    page_bindings.push(Value::Integer(limit as i64));
    page_bindings.push(Value::Integer(input.offset as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(page_bindings.iter()), parse_memory_row)?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    Ok(MemoryListResult {
        memories,
        total: total.max(0) as usize,
        limit,
        offset: input.offset,
    })
}

/// Apply a partial update to a memory.
///
/// `decay_rate` is recomputed when `category` changes, because in this crate it
/// is a pure function of category ([`get_decay_rate`]) and would otherwise go
/// stale. `vitality` and `access_count` are deliberately left alone: they encode
/// accrued retrieval history, and resetting them on an edit would discard it.
/// The reference does not recompute either — its `base_weight` is seeded from
/// `source` alone and is not category-derived.
pub fn update_memory(conn: &Connection, input: &MemoryUpdateInput) -> Result<UpdateOutcome> {
    if get_memory_by_id(conn, &input.memory_id)?.is_none() {
        return Ok(UpdateOutcome::NotFound);
    }

    let mut sets: Vec<&str> = Vec::new();
    let mut bindings: Vec<Value> = Vec::new();

    if let Some(content) = &input.content {
        sets.push("content = ?");
        bindings.push(Value::Text(content.clone()));
    }
    if let Some(category) = &input.category {
        sets.push("category = ?");
        bindings.push(Value::Text(category.clone()));
        sets.push("decay_rate = ?");
        bindings.push(Value::Real(get_decay_rate(category)));
    }
    if let Some(tags) = &input.tags {
        sets.push("tags = ?");
        bindings.push(Value::Text(
            serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string()),
        ));
    }
    if let Some(metadata) = &input.metadata {
        sets.push("metadata = ?");
        bindings.push(Value::Text(
            serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_string()),
        ));
    }

    if sets.is_empty() {
        return Ok(UpdateOutcome::NoFields);
    }

    sets.push("updated_at = ?");
    bindings.push(Value::Text(Utc::now().to_rfc3339()));
    bindings.push(Value::Text(input.memory_id.clone()));

    conn.execute(
        &format!("UPDATE memories SET {} WHERE id = ?", sets.join(", ")),
        params_from_iter(bindings.iter()),
    )?;

    let memory =
        get_memory_by_id(conn, &input.memory_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    Ok(UpdateOutcome::Updated(Box::new(memory)))
}

/// Delete a memory. Returns `false` if no live memory had that id.
///
/// This is a hard delete, matching the reference: it tombstones via `deleted_at`
/// only when sync is configured (`NODE_ID and HUB_URL and SYNC_SECRET`), so that
/// the deletion can propagate to other nodes. This crate has no sync layer, so
/// there is nothing to propagate to and the reference's own path is a plain
/// `DELETE`. The `deleted_at` column and the `deleted_at IS NULL` read filters
/// stay in place for when sync lands.
///
/// Cleanup rides on the schema: the `memories_ad` trigger removes the FTS row
/// and `memory_entities` cascades (`PRAGMA foreign_keys=ON` is set at open).
pub fn delete_memory(conn: &Connection, memory_id: &str) -> Result<bool> {
    let affected = conn.execute(
        "DELETE FROM memories WHERE id = ? AND deleted_at IS NULL",
        params![memory_id],
    )?;
    Ok(affected > 0)
}

pub fn search_memories(
    conn: &Connection,
    input: &MemorySearchInput,
) -> Result<Vec<MemorySearchResult>> {
    let weights = choose_rrf_weights(&input.query, RetrievalStrategy::Auto);

    let mut sql = String::from(
        "SELECT m.id, m.content, m.category, m.tags, m.source, m.metadata, m.created_at, m.updated_at,
                m.capture_id, m.subject, m.predicate, m.object, m.superseded_by, m.decay_rate, m.vitality,
                m.access_count, m.last_accessed_at,
                bm25(memories_fts) as fts_rank
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ? AND m.deleted_at IS NULL"
    );

    if !input.include_dormant {
        sql.push_str(&format!(" AND m.vitality >= {}", VITALITY_FLOOR));
    }
    if input.min_vitality > 0.0 {
        sql.push_str(&format!(" AND m.vitality >= {}", input.min_vitality));
    }
    if let Some(ref cat) = input.category {
        sql.push_str(&format!(" AND m.category = '{}'", cat.replace('\'', "''")));
    }

    sql.push_str(" ORDER BY bm25(memories_fts) LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![input.query, (input.limit * 2) as i64], |row| {
        let memory = parse_memory_row(row)?;
        let fts_rank: f64 = row.get("fts_rank")?;
        Ok(MemorySearchResult {
            memory,
            score: -fts_rank,
            fts_score: Some(-fts_rank),
            vec_score: None,
            vitality_score: None,
        })
    })?;

    let mut candidates = Vec::new();
    for r in rows {
        candidates.push(r?);
    }

    let mut ranked = rank_rrf(candidates, weights);
    ranked.truncate(input.limit);

    let final_results = trim_by_token_budget(ranked, input.token_budget);
    Ok(final_results)
}
