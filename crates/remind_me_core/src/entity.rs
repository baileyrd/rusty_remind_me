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

pub fn upsert_entity(conn: &Connection, input: &EntityInput) -> Result<Entity> {
    let now = Utc::now().to_rfc3339();
    let id = format!("ent_{}", sha256::digest(input.name.trim().to_lowercase()));
    let aliases_json = serde_json::to_string(&input.aliases).unwrap_or_else(|_| "[]".to_string());

    conn.execute(
        "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(name) DO UPDATE SET
            kind = COALESCE(excluded.kind, entities.kind),
            updated_at = excluded.updated_at",
        params![id, input.name.trim(), input.kind, aliases_json, now, now],
    )?;

    get_entity_by_name(conn, &input.name)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get_entity_by_name(conn: &Connection, name: &str) -> Result<Option<Entity>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, aliases, created_at, updated_at FROM entities WHERE name = ?",
    )?;
    let mut rows = stmt.query_map(params![name.trim()], |row| {
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
    })?;

    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
}
