use crate::wiki_import::slugify;
use chrono::Utc;
use rusqlite::{params, Connection, Result, Row};
use serde::{Deserialize, Serialize};

/// System pages the reference refuses to delete (`wiki.RESERVED_SLUGS`).
///
/// This crate has no on-disk wiki yet, so none of these exist to be deleted
/// today. The guard is in place so behavior does not silently change when
/// `wiki_load` / `wiki_compile` start generating them.
pub const RESERVED_SLUGS: [&str; 3] = ["index", "log", "schema"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub slug: String,
    pub title: String,
    pub content: String,
    /// One-line summary shown in the wiki index.
    pub summary: String,
    /// Source-file modification time, used by the reconcile pass. Zero for a
    /// page that has never been backed by a file.
    pub mtime: f64,
    pub updated_at: String,
}

/// Outcome of a wiki delete attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WikiDeleteOutcome {
    Deleted,
    NotFound,
    /// The slug names a reserved system page; nothing was deleted.
    Reserved,
}

const WIKI_COLUMNS: &str = "slug, title, content, summary, mtime, updated_at";

fn parse_wiki_row(row: &Row) -> Result<WikiPage> {
    Ok(WikiPage {
        slug: row.get("slug")?,
        title: row.get("title")?,
        content: row.get("content")?,
        summary: row.get("summary")?,
        mtime: row.get("mtime")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Write or replace a wiki page.
///
/// `summary` is the one-line description shown in the index. The schema has no
/// `topic` column — that was a target-only extension, dropped when the schema
/// was regenerated from `remind_me`.
pub fn write_wiki_page(
    conn: &Connection,
    slug: &str,
    title: &str,
    content: &str,
    summary: &str,
) -> Result<WikiPage> {
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO wiki_pages (slug, title, content, summary, mtime, updated_at)
         VALUES (?, ?, ?, ?, 0, ?)
         ON CONFLICT(slug) DO UPDATE SET
            title = excluded.title,
            content = excluded.content,
            summary = excluded.summary,
            updated_at = excluded.updated_at",
        params![slug, title, content, summary, now],
    )?;

    get_wiki_page(conn, slug)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get_wiki_page(conn: &Connection, slug: &str) -> Result<Option<WikiPage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM wiki_pages WHERE slug = ?",
        WIKI_COLUMNS
    ))?;
    let mut rows = stmt.query_map(params![slug], parse_wiki_row)?;

    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
}

pub fn list_wiki_pages(conn: &Connection) -> Result<Vec<WikiPage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM wiki_pages ORDER BY updated_at DESC",
        WIKI_COLUMNS
    ))?;
    let rows = stmt.query_map([], parse_wiki_row)?;

    let mut pages = Vec::new();
    for r in rows {
        pages.push(r?);
    }
    Ok(pages)
}

/// Delete a wiki page addressed by either its title or its slug.
///
/// Both forms work because the input is run through [`slugify`], which is
/// idempotent on a string that is already a slug: `"VLAN Setup!"` and
/// `"vlan-setup"` both resolve to `vlan-setup`. This is exactly how the
/// reference's `wiki.delete_page` accepts either form.
///
/// Reserved system pages ([`RESERVED_SLUGS`]) are refused rather than deleted.
pub fn delete_wiki_page(conn: &Connection, title_or_slug: &str) -> Result<WikiDeleteOutcome> {
    let slug = slugify(title_or_slug);

    if RESERVED_SLUGS.contains(&slug.as_str()) {
        return Ok(WikiDeleteOutcome::Reserved);
    }

    let affected = conn.execute("DELETE FROM wiki_pages WHERE slug = ?", params![slug])?;
    Ok(if affected > 0 {
        WikiDeleteOutcome::Deleted
    } else {
        WikiDeleteOutcome::NotFound
    })
}
