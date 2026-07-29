//! Conversation capture: a verbatim dialog and its summary, stored as two
//! linked memories.
//!
//! The pair shares a `capture_id`, which is the handle everything downstream
//! keys on — `remind_me_get_capture` to retrieve both halves, and
//! `remind_me_decompose` to break the capture into atomic facts.

use crate::db::queries::{parse_memory_row, MEMORY_COLUMNS};
use crate::models::{
    AutoCaptureInput, Capture, CaptureResult, Memory, CAPTURE_SOURCE, CAPTURE_TITLE_CHARS,
    DIALOG_CATEGORY,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use rusqlite::{params, Connection, Result};

/// Derive a display title from a summary when the caller supplied none.
///
/// First line, capped — a summary's opening line is the closest thing to a
/// headline it has.
fn derive_title(title: &str, summary: &str) -> String {
    let supplied = title.trim();
    if !supplied.is_empty() {
        return supplied.to_string();
    }
    summary
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(CAPTURE_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Merge the caller's metadata with the fields that make a capture a capture.
fn capture_metadata(
    base: &serde_json::Value,
    capture_id: &str,
    title: &str,
    kind: &str,
    link_key: &str,
    link_value: &str,
) -> serde_json::Value {
    let mut metadata = match base {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => serde_json::json!({}),
    };
    let object = metadata.as_object_mut().expect("just built an object");
    object.insert("capture_id".into(), serde_json::json!(capture_id));
    object.insert("title".into(), serde_json::json!(title));
    object.insert("type".into(), serde_json::json!(kind));
    object.insert(link_key.into(), serde_json::json!(link_value));
    metadata
}

#[allow(clippy::too_many_arguments)]
fn insert_half(
    conn: &Connection,
    id: &str,
    content: &str,
    category: &str,
    tags_json: &str,
    metadata: &serde_json::Value,
    capture_id: &str,
    now_iso: &str,
) -> Result<()> {
    let now = Utc::now();
    let decay_rate = get_decay_rate(category);
    let base_weight = get_type_prior(category) * get_source_prior(CAPTURE_SOURCE);
    let vitality = calculate_vitality(base_weight, 0, decay_rate, now_iso, now);

    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata, capture_id,
            created_at, updated_at, decay_rate, vitality, base_weight,
            access_count, accessed_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
        params![
            id,
            content,
            category,
            tags_json,
            CAPTURE_SOURCE,
            metadata.to_string(),
            capture_id,
            now_iso,
            now_iso,
            decay_rate,
            vitality,
            base_weight,
            now_iso,
        ],
    )?;
    Ok(())
}

/// Store a conversation as a linked dialog/summary pair.
///
/// The **dialog's category is always [`DIALOG_CATEGORY`]**; the caller's
/// `category` names the summary. That asymmetry is load-bearing rather than
/// incidental: `extract_batch` excludes `dialog`, because a raw transcript is
/// not a candidate for triple extraction — its facts come out through
/// `decompose` instead. Storing the transcript under the caller's category
/// would flood the annotation backlog with raw conversations.
///
/// Both halves are linked three ways: the `capture_id` column, a `capture_id`
/// in each one's metadata, and a direct pointer at the other half
/// (`linked_dialog` / `linked_summary`). The dialog's pointer is written in a
/// second pass, because the summary's id does not exist when the dialog is
/// inserted.
pub fn auto_capture(conn: &Connection, input: &AutoCaptureInput) -> Result<CaptureResult> {
    let now_iso = Utc::now().to_rfc3339();
    let capture_id = format!("cap_{}", uuid::Uuid::new_v4().simple());
    let dialog_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let summary_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let title = derive_title(&input.title, &input.summary);
    let tags_json = serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".to_string());

    let dialog_meta = capture_metadata(
        &input.metadata,
        &capture_id,
        &title,
        "dialog",
        "linked_summary",
        &summary_id,
    );
    let summary_meta = capture_metadata(
        &input.metadata,
        &capture_id,
        &title,
        "summary",
        "linked_dialog",
        &dialog_id,
    );

    insert_half(
        conn,
        &dialog_id,
        &input.conversation,
        DIALOG_CATEGORY,
        &tags_json,
        &dialog_meta,
        &capture_id,
        &now_iso,
    )?;
    insert_half(
        conn,
        &summary_id,
        &input.summary,
        &input.category,
        &tags_json,
        &summary_meta,
        &capture_id,
        &now_iso,
    )?;

    Ok(CaptureResult {
        capture_id,
        dialog_id,
        summary_id,
        title,
        tags: input.tags.clone(),
        category: input.category.clone(),
    })
}

/// Retrieve both halves of a capture.
///
/// Looks up the indexed `capture_id` column rather than scanning metadata.
/// Returns `None` when nothing carries the id.
///
/// The halves are told apart by their metadata `type`. Anything sharing the id
/// but matching neither is returned in `other` rather than dropped — a capture
/// that has lost a half, or gained a third row through sync, should be visible
/// rather than silently half-reported.
pub fn get_capture(conn: &Connection, capture_id: &str) -> Result<Option<Capture>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE capture_id = ? ORDER BY category",
        MEMORY_COLUMNS
    ))?;
    let rows: Vec<Memory> = stmt
        .query_map(params![capture_id], parse_memory_row)?
        .collect::<Result<_>>()?;
    if rows.is_empty() {
        return Ok(None);
    }

    let kind_of = |memory: &Memory| -> Option<String> {
        memory
            .metadata
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };

    let mut dialog = None;
    let mut summary = None;
    let mut other = Vec::new();
    for memory in rows {
        match kind_of(&memory).as_deref() {
            Some("dialog") if dialog.is_none() => dialog = Some(memory),
            Some("summary") if summary.is_none() => summary = Some(memory),
            _ => other.push(memory),
        }
    }

    let title = summary
        .as_ref()
        .or(dialog.as_ref())
        .and_then(|m| m.metadata.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();

    Ok(Some(Capture {
        capture_id: capture_id.to_string(),
        title,
        dialog,
        summary,
        other,
    }))
}
