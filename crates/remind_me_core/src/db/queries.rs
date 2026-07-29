use crate::fts::sanitize_fts_query;
use crate::models::{
    AnnotateInput, AnnotateResult, AnnotationApplied, AnnotationError, Memory, MemoryAddInput,
    MemoryListInput, MemoryListResult, MemorySearchInput, MemorySearchResult, MemoryUpdateInput,
    ReclassifyBatchInput, ReclassifyBatchResult, ReclassifyInput, ReclassifyResult,
    RetrievalStrategy, UnclassifiedMemory, UpdateOutcome, LIST_LIMIT_MAX, LIST_LIMIT_MIN,
    RECLASSIFY_BATCH_MAX, RECLASSIFY_BATCH_MIN, UNCLASSIFIED,
};
use crate::retrieval::{choose_rrf_weights, rank_rrf, trim_by_token_budget};
use crate::vitality::{
    calculate_vitality, get_decay_rate, get_source_prior, get_type_prior, EFFECTIVE_VITALITY_FN,
    VITALITY_FLOOR,
};
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Result, Row};

/// Columns selected wherever a full [`Memory`] is parsed via [`parse_memory_row`].
pub const MEMORY_COLUMNS: &str = "id, content, category, tags, source, metadata, created_at, \
     updated_at, capture_id, subject, predicate, object, superseded_by, decay_rate, vitality, \
     base_weight, access_count, accessed_at";

/// [`MEMORY_COLUMNS`] with each name qualified by `alias`, for queries that join.
fn prefixed_memory_columns(alias: &str) -> String {
    MEMORY_COLUMNS
        .split(',')
        .map(|c| format!("{}.{}", alias, c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn parse_memory_row(row: &Row) -> Result<Memory> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();

    let meta_json: String = row.get("metadata")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&meta_json).unwrap_or(serde_json::Value::Null);

    let created_at: String = row.get("created_at")?;

    Ok(Memory {
        id: row.get("id")?,
        content: row.get("content")?,
        category: row.get("category")?,
        tags,
        source: row.get("source")?,
        metadata,
        created_at: created_at.clone(),
        updated_at: row.get("updated_at")?,
        capture_id: row.get("capture_id")?,
        subject: row.get("subject")?,
        predicate: row.get("predicate")?,
        object: row.get("object")?,
        superseded_by: row.get("superseded_by")?,
        decay_rate: row.get("decay_rate")?,
        vitality: row.get("vitality")?,
        base_weight: row.get("base_weight")?,
        access_count: row.get("access_count")?,
        // The column is nullable and a row written by `remind_me` may leave it
        // unset. Falling back to `created_at` matches the reference, and keeps
        // `get::<String>` from failing on NULL.
        accessed_at: row
            .get::<_, Option<String>>("accessed_at")?
            .unwrap_or_else(|| created_at.clone()),
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
            subject, predicate, object, decay_rate, vitality, base_weight, access_count, accessed_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
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
            base_weight,
            now_iso,
        ],
    )?;

    // `MemoryAddInput::entities` was previously parsed and then dropped, so a
    // caller supplying entity mentions got a silent no-op. Same path as
    // `annotate_memories` so both behave identically.
    crate::entity::apply_entity_mentions(conn, &id, &input.entities)?;

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
/// Tag matching is ALL-of, one `EXISTS` per requested tag, against the
/// normalized `memory_tags` index. That table is kept in step with the JSON
/// `tags` column by the `memories_tags_ai` / `_au` / `_ad` triggers, so the two
/// representations cannot drift.
///
/// The predicate stays in SQL rather than filtering parsed rows in Rust, which
/// is what lets `COUNT`, `LIMIT` and `OFFSET` stay correct — the reference hit
/// exactly that pagination bug as `DATA-02`. This previously scanned
/// `json_each(m.tags)` for the same reason, before `memory_tags` existed;
/// `idx_memory_tags_tag` now serves it without parsing JSON per row.
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
        conditions.push(
            "EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)"
                .to_string(),
        );
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
/// Deliberately does **not** touch `decay_rate`. An earlier version recomputed
/// it whenever `category` changed, which introduced a second writer: decay is
/// derived from `memory_type`, and [`reclassify_memories`] owns it. With both
/// writing, reclassifying a memory to `decision` and then editing its category
/// to `action_item` would silently contradict the classification. The reference
/// never touches `decay_rate` on update for exactly this reason.
///
/// `vitality`, `base_weight` and `access_count` are left alone too: they encode
/// accrued retrieval history, and resetting them on an edit would discard it.
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
/// The FTS row and `memory_tags` are handled by triggers. Everything else is
/// cleaned up explicitly, because the reference's schema carries **no foreign
/// keys** on `memory_entities`, `memory_feedback` or `memory_associations` —
/// deliberately, since sync can deliver a link before the memory it points at,
/// and a cascade would reject that. This crate previously relied on a cascade
/// it had added itself; regenerating the schema from `remind_me` removed it.
pub fn delete_memory(conn: &Connection, memory_id: &str) -> Result<bool> {
    let affected = conn.execute(
        "DELETE FROM memories WHERE id = ? AND deleted_at IS NULL",
        params![memory_id],
    )?;
    if affected == 0 {
        return Ok(false);
    }

    // Entities themselves survive — other memories may still mention them.
    for table in ["memory_entities", "memory_feedback"] {
        conn.execute(
            &format!("DELETE FROM {} WHERE memory_id = ?", table),
            params![memory_id],
        )?;
    }
    conn.execute(
        "DELETE FROM memory_associations WHERE memory_id_a = ? OR memory_id_b = ?",
        params![memory_id, memory_id],
    )?;

    Ok(true)
}

/// Apply a batch of annotations: SPO triple fields and entity mentions.
///
/// Per-item error handling, matching the reference: an unknown `memory_id` is
/// recorded in `errors` and the rest of the batch still applies. An
/// all-or-nothing transaction would mean one stale id from an extraction pass
/// discards up to 99 good annotations.
///
/// Only the SPO fields actually supplied are written; omitted ones keep their
/// current value. `updated_at` moves whenever an annotation is applied, even if
/// it only added entity mentions.
pub fn annotate_memories(conn: &Connection, input: &AnnotateInput) -> Result<AnnotateResult> {
    let now = Utc::now().to_rfc3339();
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for annotation in &input.annotations {
        if get_memory_by_id(conn, &annotation.memory_id)?.is_none() {
            errors.push(AnnotationError {
                memory_id: annotation.memory_id.clone(),
                error: "memory not found".to_string(),
            });
            continue;
        }

        let mut sets: Vec<&str> = Vec::new();
        let mut bindings: Vec<Value> = Vec::new();
        for (column, value) in [
            ("subject = ?", &annotation.subject),
            ("predicate = ?", &annotation.predicate),
            ("object = ?", &annotation.object),
        ] {
            if let Some(v) = value {
                sets.push(column);
                bindings.push(Value::Text(v.clone()));
            }
        }

        sets.push("updated_at = ?");
        bindings.push(Value::Text(now.clone()));
        bindings.push(Value::Text(annotation.memory_id.clone()));

        conn.execute(
            &format!("UPDATE memories SET {} WHERE id = ?", sets.join(", ")),
            params_from_iter(bindings.iter()),
        )?;

        let entities_linked = crate::entity::apply_entity_mentions(
            conn,
            &annotation.memory_id,
            &annotation.entities,
        )?;

        results.push(AnnotationApplied {
            memory_id: annotation.memory_id.clone(),
            entities_linked,
        });
    }

    Ok(AnnotateResult { results, errors })
}

/// Apply memory-type classifications, updating the decay rate to match.
///
/// `decay_rate` is a pure function of `memory_type` ([`get_decay_rate`]), and
/// this is the only place that writes it. Idempotent: reclassifying overwrites
/// the previous type and rate.
///
/// Unknown ids are collected into `not_found` rather than failing the batch —
/// a classification pass over stale ids should not discard its good work.
/// Vitality and `base_weight` are untouched; classification says what a memory
/// *is*, not how much it has been used.
pub fn reclassify_memories(conn: &Connection, input: &ReclassifyInput) -> Result<ReclassifyResult> {
    let now = Utc::now().to_rfc3339();
    let mut updated = 0;
    let mut not_found = Vec::new();

    for classification in &input.classifications {
        if get_memory_by_id(conn, &classification.memory_id)?.is_none() {
            not_found.push(classification.memory_id.clone());
            continue;
        }

        conn.execute(
            "UPDATE memories SET memory_type = ?, decay_rate = ?, updated_at = ? WHERE id = ?",
            params![
                classification.memory_type,
                get_decay_rate(&classification.memory_type),
                now,
                classification.memory_id
            ],
        )?;
        updated += 1;
    }

    Ok(ReclassifyResult {
        updated,
        not_found,
        total: input.classifications.len(),
    })
}

/// Fetch memories still awaiting classification, with a snippet for review.
///
/// `total_unclassified` counts every remaining memory, not just this page, so a
/// caller can tell whether another round is worth requesting.
pub fn unclassified_batch(
    conn: &Connection,
    input: &ReclassifyBatchInput,
) -> Result<ReclassifyBatchResult> {
    let batch_size = input
        .batch_size
        .clamp(RECLASSIFY_BATCH_MIN, RECLASSIFY_BATCH_MAX);

    let total_unclassified: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE memory_type = ? AND deleted_at IS NULL",
        params![UNCLASSIFIED],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, substr(content, 1, 500), category, tags
           FROM memories
          WHERE memory_type = ? AND deleted_at IS NULL
          ORDER BY created_at, id
          LIMIT ?",
    )?;
    let rows = stmt.query_map(params![UNCLASSIFIED, batch_size as i64], |row| {
        let tags_json: String = row.get(3)?;
        Ok(UnclassifiedMemory {
            id: row.get(0)?,
            content_snippet: row.get(1)?,
            category: row.get(2)?,
            tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        })
    })?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    Ok(ReclassifyBatchResult {
        memories,
        total_unclassified: total_unclassified.max(0) as usize,
    })
}

/// Effective vitality as a SQL expression over the alias `m`.
///
/// The stored `vitality` column is a write-time snapshot that never decays, so
/// filtering on it means `include_dormant: false` filters nothing and
/// `min_vitality` compares against a number unrelated to the memory's current
/// standing. This calls [`vitality::register_sql_functions`]'s scalar function
/// instead, which keeps the predicate *before* `LIMIT`.
///
/// Doing it in Rust after the fetch would push the filter after the limit, the
/// shape the reference warns about as `DI-03`: the page would be truncated
/// first and then thinned, under-filling every result set.
///
/// `coalesce`s `accessed_at` to `created_at`, because a memory that has never
/// been retrieved should age from when it was written.
fn effective_vitality_sql() -> String {
    format!(
        "{}(m.base_weight, m.access_count, m.decay_rate, coalesce(m.accessed_at, m.created_at))",
        EFFECTIVE_VITALITY_FN
    )
}

pub fn search_memories(
    conn: &Connection,
    input: &MemorySearchInput,
) -> Result<Vec<MemorySearchResult>> {
    let weights = choose_rrf_weights(&input.query, RetrievalStrategy::Auto);

    // Raw user text is not a valid FTS5 MATCH expression — ordinary punctuation
    // is operator syntax there, so a question like "what's the plan?" was a
    // syntax error rather than a search. An empty result means nothing was
    // searchable; MATCH on an empty string is itself an error.
    let match_expr = sanitize_fts_query(&input.query);
    if match_expr.is_empty() {
        return Ok(Vec::new());
    }

    // Derived from MEMORY_COLUMNS rather than spelled out, so adding a column
    // cannot leave this query selecting a stale subset — which is exactly how
    // `base_weight` slipped past here once.
    let mut sql = format!(
        "SELECT {}, bm25(memories_fts) as fts_rank
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ? AND m.deleted_at IS NULL",
        prefixed_memory_columns("m")
    );

    let effective = effective_vitality_sql();
    if !input.include_dormant {
        sql.push_str(&format!(" AND {} >= {}", effective, VITALITY_FLOOR));
    }
    if input.min_vitality > 0.0 {
        sql.push_str(&format!(" AND {} >= {}", effective, input.min_vitality));
    }
    if let Some(ref cat) = input.category {
        sql.push_str(&format!(" AND m.category = '{}'", cat.replace('\'', "''")));
    }

    sql.push_str(" ORDER BY bm25(memories_fts) LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![match_expr, (input.limit * 2) as i64], |row| {
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
