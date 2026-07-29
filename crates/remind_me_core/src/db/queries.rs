use crate::models::{
    Memory, MemoryAddInput, MemorySearchInput, MemorySearchResult, RetrievalStrategy,
};
use crate::retrieval::{choose_rrf_weights, rank_rrf, trim_by_token_budget};
use crate::vitality::{
    calculate_vitality, get_decay_rate, get_source_prior, get_type_prior, VITALITY_FLOOR,
};
use chrono::Utc;
use rusqlite::{params, Connection, Result, Row};

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
    let mut stmt = conn.prepare(
        "SELECT id, content, category, tags, source, metadata, created_at, updated_at,
                capture_id, subject, predicate, object, superseded_by, decay_rate, vitality,
                access_count, last_accessed_at
         FROM memories WHERE id = ? AND deleted_at IS NULL",
    )?;

    let mut rows = stmt.query_map(params![id], parse_memory_row)?;
    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
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
