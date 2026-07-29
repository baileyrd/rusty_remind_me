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

/// Normalise an entity name for identity: lowercased, whitespace collapsed.
///
/// Collapsing *internal* runs matters as much as trimming the ends —
/// `"Bailey  Robertson"` and `"bailey robertson"` name the same person, and the
/// id is derived from this form so they resolve to one row. An earlier version
/// only trimmed, which made them two entities here and one in `remind_me`.
///
/// Shared by every path that needs an entity's identity, so no caller can
/// normalise differently.
pub fn normalize_entity_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The deterministic id for an entity name.
///
/// Content hash, no timestamp: two machines that independently record the same
/// entity converge on the same row rather than conflicting. That only works if
/// both derive the id identically, so this mirrors `remind_me`'s `_entity_id`
/// exactly — sha256 of the normalised name, truncated to 12 hex characters,
/// unprefixed.
///
/// Twelve hex characters is 48 bits. That collision domain is inherited from
/// the reference rather than chosen here; widening it would break interop,
/// which is the whole reason the id is derived at all.
pub fn entity_id(name: &str) -> String {
    sha256::digest(normalize_entity_name(name))[..12].to_string()
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
    let id = entity_id(name);

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
        "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id, created_at)
         VALUES (?, ?, ?)",
        params![memory_id, entity_id, Utc::now().to_rfc3339()],
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
    get_entity_by_id(conn, &entity_id(name))
}

/// Rewrite entity ids that predate [`entity_id`]'s current derivation.
///
/// Returns the number of rows rewritten. Idempotent: a database whose ids
/// already match is untouched, so this is safe to run on every open.
///
/// Ids used to be `ent_` plus the full 64-hex digest of a merely-trimmed name.
/// The reference uses the first 12 hex characters of the digest of a
/// whitespace-collapsed name, with no prefix, so every entity written by this
/// crate was invisible to `remind_me` and vice versa.
///
/// Nothing cascades — the reference declares no foreign key on `memory_entities`
/// or `entity_relations`, so that sync can deliver rows out of order — which
/// means the referencing columns have to be repointed explicitly or every link
/// dangles.
///
/// Two rows can collapse onto one id, because names differing only by internal
/// whitespace used to be distinct entities. Those are merged rather than left
/// to collide on the primary key: aliases union, the earliest `created_at`
/// wins, and a `kind` already set is kept.
pub fn renormalize_entity_ids(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(&format!("{} ORDER BY created_at, id", ENTITY_SELECT))?;
    let existing: Vec<Entity> = stmt
        .query_map([], parse_entity_row)?
        .collect::<Result<_>>()?;
    drop(stmt);

    let mut rewritten = 0;
    for entity in existing {
        let want = entity_id(&entity.name);
        if want == entity.id {
            continue;
        }

        match get_entity_by_id(conn, &want)? {
            None => {
                conn.execute(
                    "UPDATE entities SET id = ? WHERE id = ?",
                    params![want, entity.id],
                )?;
            }
            Some(target) => {
                let merged = dedup_preserving_order(
                    target.aliases.iter().cloned().chain(entity.aliases.clone()),
                );
                let kind = target.kind.clone().or_else(|| entity.kind.clone());
                let created_at = target.created_at.min(entity.created_at.clone());
                conn.execute(
                    "UPDATE entities SET kind = ?, aliases = ?, created_at = ? WHERE id = ?",
                    params![
                        kind,
                        serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string()),
                        created_at,
                        want
                    ],
                )?;
                conn.execute("DELETE FROM entities WHERE id = ?", params![entity.id])?;
            }
        }

        // `memory_entities` is keyed `(memory_id, entity_id)`, so repointing can
        // collide with a link the surviving entity already has. Ignore those,
        // then drop whatever the ignore left behind.
        conn.execute(
            "UPDATE OR IGNORE memory_entities SET entity_id = ? WHERE entity_id = ?",
            params![want, entity.id],
        )?;
        conn.execute(
            "DELETE FROM memory_entities WHERE entity_id = ?",
            params![entity.id],
        )?;
        // Relations are keyed on their own id, so these cannot collide.
        conn.execute(
            "UPDATE entity_relations SET subject_entity_id = ? WHERE subject_entity_id = ?",
            params![want, entity.id],
        )?;
        conn.execute(
            "UPDATE entity_relations SET object_entity_id = ? WHERE object_entity_id = ?",
            params![want, entity.id],
        )?;

        rewritten += 1;
    }

    Ok(rewritten)
}
