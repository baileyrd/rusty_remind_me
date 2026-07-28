use chrono::Utc;
use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub slug: String,
    pub title: String,
    pub content: String,
    pub topic: String,
    pub created_at: String,
    pub updated_at: String,
}

pub fn write_wiki_page(
    conn: &Connection,
    slug: &str,
    title: &str,
    content: &str,
    topic: &str,
) -> Result<WikiPage> {
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO wiki_pages (slug, title, content, topic, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(slug) DO UPDATE SET
            title = excluded.title,
            content = excluded.content,
            topic = excluded.topic,
            updated_at = excluded.updated_at",
        params![slug, title, content, topic, now, now],
    )?;

    get_wiki_page(conn, slug)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get_wiki_page(conn: &Connection, slug: &str) -> Result<Option<WikiPage>> {
    let mut stmt = conn.prepare("SELECT slug, title, content, topic, created_at, updated_at FROM wiki_pages WHERE slug = ?")?;
    let mut rows = stmt.query_map(params![slug], |row| {
        Ok(WikiPage {
            slug: row.get("slug")?,
            title: row.get("title")?,
            content: row.get("content")?,
            topic: row.get("topic")?,
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

pub fn list_wiki_pages(conn: &Connection) -> Result<Vec<WikiPage>> {
    let mut stmt = conn.prepare("SELECT slug, title, content, topic, created_at, updated_at FROM wiki_pages ORDER BY updated_at DESC")?;
    let rows = stmt.query_map([], |row| {
        Ok(WikiPage {
            slug: row.get("slug")?,
            title: row.get("title")?,
            content: row.get("content")?,
            topic: row.get("topic")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    })?;

    let mut pages = Vec::new();
    for r in rows {
        pages.push(r?);
    }
    Ok(pages)
}
