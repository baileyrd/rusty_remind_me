use crate::models::EntityInput;
use chrono::Utc;
use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Insert an entity, or merge into the existing row of the same name.
///
/// Aliases **union-merge**: existing first, then new ones, de-duplicated and
/// order-preserving. A missing `kind` is filled in, but an existing `kind` is
/// never overwritten — the reference resolves this the same way (`row["kind"] or
/// kind`), so a later mention that guesses a different kind cannot clobber a
/// deliberate earlier one.
///
/// `updated_at` moves only when something actually changed, so a no-op mention
/// does not churn the row.
pub fn upsert_entity(conn: &Connection, input: &EntityInput) -> Result<Entity> {
    let now = Utc::now().to_rfc3339();
    let name = input.name.trim();
    let id = format!("ent_{}", sha256::digest(name.to_lowercase()));

    let clean_aliases: Vec<String> = dedup_preserving_order(
        input
            .aliases
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty()),
    );

    // Key on the derived id, not on `name`. The id is the identity — it is
    // built from the case-folded name precisely so "Tasmania" and "tasmania"
    // are one entity. Matching on the `name` column instead is case-sensitive,
    // so a casing variant misses the lookup, tries to insert, and collides on
    // the `entities.id` unique constraint.
    match get_entity_by_id(conn, &id)? {
        None => {
            conn.execute(
                "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    name,
                    input.kind,
                    serde_json::to_string(&clean_aliases).unwrap_or_else(|_| "[]".to_string()),
                    now,
                    now
                ],
            )?;
        }
        Some(existing) => {
            let merged = dedup_preserving_order(
                existing
                    .aliases
                    .iter()
                    .cloned()
                    .chain(clean_aliases.clone()),
            );
            // Existing kind wins; `input.kind` only fills a hole.
            let new_kind = existing.kind.clone().or_else(|| input.kind.clone());

            if merged != existing.aliases || new_kind != existing.kind {
                conn.execute(
                    "UPDATE entities SET kind = ?, aliases = ?, updated_at = ? WHERE id = ?",
                    params![
                        new_kind,
                        serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string()),
                        now,
                        id
                    ],
                )?;
            }
        }
    }

    get_entity_by_id(conn, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// Fetch an entity by its deterministic id.
pub fn get_entity_by_id(conn: &Connection, id: &str) -> Result<Option<Entity>> {
    let mut stmt = conn.prepare(&format!("{} WHERE id = ?", ENTITY_SELECT))?;
    let mut rows = stmt.query_map(params![id], parse_entity_row)?;
    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
}

fn dedup_preserving_order<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// Record that a memory mentions an entity. Returns `true` if the link is new.
///
/// Insert-or-ignore: mention links are immutable, and re-annotating with the
/// same entity is a no-op rather than an error.
pub fn link_memory_entity(conn: &Connection, memory_id: &str, entity_id: &str) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id) VALUES (?, ?)",
        params![memory_id, entity_id],
    )?;
    Ok(inserted > 0)
}

/// Upsert each mentioned entity and link it to `memory_id`.
///
/// Returns the number of **new** links created; entities already linked to this
/// memory are counted as zero. Shared by `add_memory` and `remind_me_annotate`
/// so both apply mentions identically.
pub fn apply_entity_mentions(
    conn: &Connection,
    memory_id: &str,
    entities: &[EntityInput],
) -> Result<usize> {
    let mut linked = 0;
    for input in entities {
        if input.name.trim().is_empty() {
            continue;
        }
        let entity = upsert_entity(conn, input)?;
        if link_memory_entity(conn, memory_id, &entity.id)? {
            linked += 1;
        }
    }
    Ok(linked)
}

const ENTITY_SELECT: &str = "SELECT id, name, kind, aliases, created_at, updated_at FROM entities";

fn parse_entity_row(row: &rusqlite::Row) -> Result<Entity> {
    let aliases_json: String = row.get("aliases")?;
    let aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
    Ok(Entity {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        aliases,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Fetch an entity by name, case- and whitespace-insensitively.
///
/// Resolves through the derived id rather than matching the `name` column, so
/// `"tasmania"` finds the entity stored as `"Tasmania"` — the same identity the
/// id encodes.
pub fn get_entity_by_name(conn: &Connection, name: &str) -> Result<Option<Entity>> {
    let id = format!("ent_{}", sha256::digest(name.trim().to_lowercase()));
    get_entity_by_id(conn, &id)
}
