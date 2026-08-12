//! Concrete `Storage` implementation over SQLite.
//!
//! Mirrors `SqliteStorage` in `src/dbs/storage/sqlite.py` in
//! baileyrd/Daily-Backup-System (pinned `@6cc6491`). Issue #36 is large
//! (~1100 lines in the reference) and its own acceptance checklist calls
//! for splitting the work across multiple PRs by trait section. Landed:
//! schema lifecycle, sources, runs, cursor/state, locking (first PR);
//! items/batch commit including media archiving (second PR); and
//! **export/browse/stats/maintenance** (this PR, closing #36).
//!
//! Differences from the reference, all deliberate:
//! * The reference's `transaction()` context manager isn't ported (see
//!   #11's note on why `Storage` has no such combinator) — each method
//!   here opens its own `rusqlite` transaction directly.
//! * `open_connection` (#12) already runs migrations, so `migrate()` here
//!   is a redundant-but-idempotent second call, matching the trait's
//!   "idempotent" contract rather than the reference's constructor/
//!   `migrate()` split.
//! * `close()` cannot truly close-and-invalidate a Rust value in place;
//!   it best-effort runs `PRAGMA optimize` (mirroring the reference) and
//!   otherwise relies on `Drop` to close the connection.
//! * The reference's `finish_run` accepts `items_failed`; the `Storage`
//!   trait's `finish_run` does not expose that parameter (see #11), so
//!   the column keeps whatever `upsert_items`/schema default left it at.
//! * `PreparedItem::media` holds each `MediaRef` (#4/#17) round-tripped
//!   through `serde_json::Value` — `upsert_items` deserializes each entry
//!   back into a typed `MediaRef` rather than pulling fields out of the
//!   `Value` by hand.
//! * The reference's media resolution only ever reads **local files** off
//!   `url` (a URL is left as a bare reference in v1); connector-prefetched
//!   bytes travel via `MediaRef::data` instead of a dict `"data"` key —
//!   same behavior, typed field.
//! * `iter_items`/`iter_revisions`/`iter_media_blobs` collect their whole
//!   result set into a `Vec` before returning it as an iterator, rather
//!   than the reference's lazy `sqlite3` cursor generator. A `Box<dyn
//!   Iterator + 'a>` borrowing a live `rusqlite::Statement`/`Rows` across
//!   the call is a self-referential-lifetime problem this port doesn't
//!   take on; export result sets are bounded by what a single backup run
//!   holds anyway. Revisit if a real streaming need surfaces.
//! * `ItemRow` (`HashMap<String, Value>`) has no binary variant, so
//!   `get_media_blob`/`iter_media_blobs` encode blob bytes as a JSON
//!   array of byte values (`serde_json`'s default `Vec<u8>` encoding)
//!   rather than the reference's raw Python `bytes` — no `base64`
//!   dependency added for a row type already documented as "kept loose
//!   on purpose" (#11).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::errors::DbsError;
use crate::models::{Cursor, MediaRef};
use crate::storage::sqlite::open_connection;
use crate::storage::{BatchResult, ExportQuery, ItemRow, PreparedItem, SourceRecord, Storage};
use crate::timeutil::{iso_z, parse_iso};

/// SQLite's default variable limit is comfortably above this; matches the
/// reference's own chunk size for `IN (...)` clauses built from a
/// caller-controlled list.
const CHUNK_SIZE: usize = 400;

/// Restricts a query to items carrying a given tag (`tags_json` is a JSON
/// array of strings) — the storage half of a tag-scoped reconcile sweep.
/// Every call site binds the tag as the query's second parameter (`?2`).
const TAG_FILTER: &str =
    " AND EXISTS (SELECT 1 FROM json_each(items.tags_json) WHERE json_each.value = ?2)";

/// Builds a `WHERE`-ready clause (always starting `1=1`, so callers can
/// unconditionally append `AND ...`) and its bound parameters from an
/// [`ExportQuery`], aliasing the `items` table as `i`. Mirrors the
/// reference's `_build_filter`, narrowed to the fields this port's
/// simplified `ExportQuery` placeholder actually has (see #11's
/// module doc-comment on why it's a placeholder).
fn build_filter(query: &ExportQuery) -> (String, Vec<SqlValue>) {
    let mut clauses = vec!["1=1".to_string()];
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(source_id) = query.source_id {
        clauses.push("i.source_id = ?".to_string());
        params.push(SqlValue::Integer(source_id));
    }
    if let Some(kind) = &query.item_kind {
        clauses.push("i.item_kind = ?".to_string());
        params.push(SqlValue::Text(kind.clone()));
    }
    if let Some(since) = query.since {
        clauses.push("i.item_created_at >= ?".to_string());
        params.push(SqlValue::Text(iso_z(since)));
    }
    if let Some(until) = query.until {
        clauses.push("i.item_created_at <= ?".to_string());
        params.push(SqlValue::Text(iso_z(until)));
    }
    if !query.include_deleted {
        clauses.push("i.deleted = 0".to_string());
    }
    (clauses.join(" AND "), params)
}

/// Escapes SQL `LIKE` wildcards in free-text search input (paired with
/// `ESCAPE '\\'`).
fn like_pattern(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// A safe FTS5 `MATCH` expression built from free user text.
///
/// Each whitespace token becomes a quoted phrase (so MATCH operators
/// like `AND`/`NOT`/`*`/`:` in user input can't change the query's
/// meaning), implicitly ANDed; the final token gets a `*` prefix-match
/// so typing "hell" still finds "Hello". Embedded quotes are doubled per
/// FTS5 escaping rules. Matches the reference's `_fts_match_query`.
fn fts_match_query(text: &str) -> String {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(|t| t.replace('"', "\"\""))
        .collect();
    if tokens.is_empty() {
        return "\"\"".to_string();
    }
    let mut quoted: Vec<String> = tokens.iter().map(|t| format!("\"{t}\"")).collect();
    let last = quoted.len() - 1;
    quoted[last].push('*');
    quoted.join(" ")
}

/// The subset of an existing `items` row `upsert_items` needs to decide
/// how to classify an incoming [`PreparedItem`] against it.
#[derive(Debug, Clone)]
struct ExistingRow {
    id: i64,
    content_hash: String,
    revision: i64,
    deleted: bool,
}

fn is_memory_path(path: &str) -> bool {
    path.is_empty() || path == ":memory:" || path.starts_with("file::memory:")
}

fn storage_err(context: &str, e: rusqlite::Error) -> DbsError {
    DbsError::Storage(format!("{context}: {e}"))
}

/// A SQLite-backed [`Storage`]. See the module doc-comment for what this
/// PR does and doesn't implement yet.
pub struct SqliteStorage {
    path: String,
    conn: Connection,
    /// Set by [`Self::ensure_fts`] — `false` degrades `browse_items`'s
    /// text search to the `LIKE`-only path (see #47).
    fts_enabled: bool,
}

impl SqliteStorage {
    /// Opens (creating/migrating as needed) a database at `path`, or a
    /// private in-memory database for `":memory:"`/`""`/`file::memory:`.
    pub fn open(path: &str) -> Result<Self, DbsError> {
        let conn = open_connection(path)?;
        let mut storage = Self {
            path: path.to_string(),
            conn,
            fts_enabled: false,
        };
        storage.fts_enabled = storage.ensure_fts()?;
        Ok(storage)
    }

    fn now(&self) -> String {
        iso_z(Utc::now())
    }

    /// Creates/refreshes the FTS5 index over `items(title, body)`.
    ///
    /// Deliberately **not** a numbered migration (see
    /// `storage::migrations`): a build of SQLite without the FTS5 module
    /// would fail a migration permanently, whereas this ensure-step just
    /// returns `false` and `browse_items` falls back to `LIKE` — matches
    /// the reference's `_ensure_fts`. External-content table + triggers
    /// keep the index in sync with every write path; the backfill runs
    /// once (index empty, `items` not).
    fn ensure_fts(&mut self) -> Result<bool, DbsError> {
        let existed: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='items_fts'",
                [],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| storage_err("failed to check for an existing FTS5 index", e))?
            .unwrap_or(false);

        if self
            .conn
            .execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(\
                     title, body, content='items', content_rowid='id')",
                [],
            )
            .is_err()
        {
            // SQLite built without the FTS5 module — degrade safely.
            return Ok(false);
        }

        self.conn
            .execute_batch(
                "CREATE TRIGGER IF NOT EXISTS items_fts_ai AFTER INSERT ON items BEGIN \
                     INSERT INTO items_fts(rowid, title, body) VALUES (new.id, new.title, new.body); \
                 END;
                 CREATE TRIGGER IF NOT EXISTS items_fts_ad AFTER DELETE ON items BEGIN \
                     INSERT INTO items_fts(items_fts, rowid, title, body) \
                     VALUES ('delete', old.id, old.title, old.body); \
                 END;
                 CREATE TRIGGER IF NOT EXISTS items_fts_au AFTER UPDATE OF title, body ON items BEGIN \
                     INSERT INTO items_fts(items_fts, rowid, title, body) \
                     VALUES ('delete', old.id, old.title, old.body); \
                     INSERT INTO items_fts(rowid, title, body) VALUES (new.id, new.title, new.body); \
                 END;",
            )
            .map_err(|e| storage_err("failed to create FTS5 sync triggers", e))?;

        if !existed {
            // First enable on a pre-FTS database: build the index from
            // the existing rows. ('rebuild' is FTS5's own backfill for
            // external-content tables — a bare COUNT can't detect
            // emptiness here, since reads pass through to the content
            // table.)
            let tx = self
                .conn
                .transaction()
                .map_err(|e| storage_err("failed to begin FTS5 backfill transaction", e))?;
            tx.execute("INSERT INTO items_fts(items_fts) VALUES('rebuild')", [])
                .map_err(|e| storage_err("failed to backfill the FTS5 index", e))?;
            tx.commit()
                .map_err(|e| storage_err("failed to commit the FTS5 backfill", e))?;
        }
        Ok(true)
    }
}

impl Storage for SqliteStorage {
    fn migrate(&mut self) -> Result<(), DbsError> {
        crate::storage::migrations::migrate(&mut self.conn)?;
        self.fts_enabled = self.ensure_fts()?;
        Ok(())
    }

    fn close(&mut self) {
        let _ = self.conn.execute("PRAGMA optimize", []);
    }

    fn spawn(&self) -> Option<Box<dyn Storage>> {
        if is_memory_path(&self.path) {
            return None;
        }
        Self::open(&self.path)
            .ok()
            .map(|s| Box::new(s) as Box<dyn Storage>)
    }

    // -- sources ------------------------------------------------------------

    fn upsert_source(
        &mut self,
        name: &str,
        type_: &str,
        plugin_id: &str,
        config_json: &str,
        schema_version: u32,
    ) -> Result<SourceRecord, DbsError> {
        let now = self.now();
        self.conn
            .execute(
                "INSERT INTO sources(name, type, plugin_id, config_json, schema_version, enabled, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                 ON CONFLICT(name) DO UPDATE SET
                     type=excluded.type,
                     plugin_id=excluded.plugin_id,
                     config_json=excluded.config_json,
                     schema_version=excluded.schema_version",
                params![name, type_, plugin_id, config_json, schema_version, now],
            )
            .map_err(|e| storage_err("failed to upsert source", e))?;
        self.get_source(name)?
            .ok_or_else(|| DbsError::Storage(format!("source {name:?} vanished after upsert")))
    }

    fn get_source(&self, name: &str) -> Result<Option<SourceRecord>, DbsError> {
        self.conn
            .query_row(
                "SELECT * FROM sources WHERE name=?1",
                params![name],
                row_to_source,
            )
            .optional()
            .map_err(|e| storage_err("failed to read source", e))
    }

    fn list_sources(&self) -> Result<Vec<SourceRecord>, DbsError> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sources ORDER BY name")
            .map_err(|e| storage_err("failed to prepare list_sources", e))?;
        let rows = stmt
            .query_map([], row_to_source)
            .map_err(|e| storage_err("failed to list sources", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read source row", e))
    }

    fn delete_source(&mut self, name: &str) -> Result<bool, DbsError> {
        let affected = self
            .conn
            .execute("DELETE FROM sources WHERE name=?1", params![name])
            .map_err(|e| storage_err("failed to delete source", e))?;
        Ok(affected > 0)
    }

    // -- runs -----------------------------------------------------------------

    fn begin_run(
        &mut self,
        source_id: i64,
        plugin_id: &str,
        mode: &str,
        cursor_before: Option<&str>,
    ) -> Result<i64, DbsError> {
        let now = self.now();
        self.conn
            .execute(
                "INSERT INTO sync_runs(source_id, plugin_id, status, mode, started_at, cursor_before)
                 VALUES (?1, ?2, 'running', ?3, ?4, ?5)",
                params![source_id, plugin_id, mode, now, cursor_before],
            )
            .map_err(|e| storage_err("failed to begin run", e))?;
        Ok(self.conn.last_insert_rowid())
    }

    fn finish_run(
        &mut self,
        run_id: i64,
        status: &str,
        stats: &BatchResult,
        items_seen: u64,
        cursor_after: Option<&str>,
        error: Option<&str>,
        warnings: &[String],
    ) -> Result<(), DbsError> {
        let now = self.now();
        let duration_ms = self.run_duration_ms(run_id, &now)?;
        let warnings_json = if warnings.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(warnings)
                    .map_err(|e| DbsError::Storage(format!("failed to encode warnings: {e}")))?,
            )
        };
        self.conn
            .execute(
                "UPDATE sync_runs SET
                     status=?1, finished_at=?2, items_seen=?3, items_created=?4,
                     items_updated=?5, items_unchanged=?6, items_deleted=?7,
                     items_undeleted=?8, revisions=?9, cursor_after=?10, error=?11,
                     warnings=?12, duration_ms=?13
                 WHERE id=?14",
                params![
                    status,
                    now,
                    items_seen,
                    stats.created,
                    stats.updated,
                    stats.unchanged,
                    stats.deleted,
                    stats.undeleted,
                    stats.revisions,
                    cursor_after,
                    error,
                    warnings_json,
                    duration_ms,
                    run_id,
                ],
            )
            .map_err(|e| storage_err("failed to finish run", e))?;
        Ok(())
    }

    fn reap_interrupted_runs(&mut self) -> Result<Vec<i64>, DbsError> {
        let now = self.now();
        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("failed to begin reap transaction", e))?;
        let ids: Vec<i64> = {
            let mut stmt = tx
                .prepare("SELECT id FROM sync_runs WHERE status='running'")
                .map_err(|e| storage_err("failed to prepare reap select", e))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(|e| storage_err("failed to list running runs", e))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| storage_err("failed to read running run id", e))?
        };
        if !ids.is_empty() {
            tx.execute(
                "UPDATE sync_runs SET status='interrupted', finished_at=?1 WHERE status='running'",
                params![now],
            )
            .map_err(|e| storage_err("failed to mark runs interrupted", e))?;
        }
        tx.execute(
            "DELETE FROM source_locks WHERE run_id NOT IN
             (SELECT id FROM sync_runs WHERE status='running')",
            [],
        )
        .map_err(|e| storage_err("failed to clear stale locks", e))?;
        tx.commit()
            .map_err(|e| storage_err("failed to commit reap transaction", e))?;
        Ok(ids)
    }

    fn recent_runs(&self, source_id: Option<i64>, limit: u32) -> Result<Vec<ItemRow>, DbsError> {
        let sql = if source_id.is_some() {
            "SELECT r.*, s.name AS source_name FROM sync_runs r \
             JOIN sources s ON s.id = r.source_id \
             WHERE r.source_id=?1 ORDER BY r.started_at DESC, r.id DESC LIMIT ?2"
        } else {
            "SELECT r.*, s.name AS source_name FROM sync_runs r \
             JOIN sources s ON s.id = r.source_id \
             ORDER BY r.started_at DESC, r.id DESC LIMIT ?1"
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| storage_err("failed to prepare recent_runs", e))?;
        let rows_iter = if let Some(sid) = source_id {
            stmt.query_map(params![sid, limit], row_to_run)
        } else {
            stmt.query_map(params![limit], row_to_run)
        }
        .map_err(|e| storage_err("failed to list recent runs", e))?;
        rows_iter
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read run row", e))
    }

    // -- items / batch commit -------------------------------------------------

    fn upsert_items(
        &mut self,
        source_id: i64,
        run_id: i64,
        items: &[PreparedItem],
        store_media: bool,
        max_media_bytes: u64,
    ) -> Result<BatchResult, DbsError> {
        let mut res = BatchResult::default();
        if items.is_empty() {
            return Ok(res);
        }
        let now = self.now();
        let external_ids: Vec<&str> = items.iter().map(|it| it.external_id.as_str()).collect();
        let mut existing = self.existing_index(source_id, &external_ids)?;

        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("failed to begin upsert_items transaction", e))?;
        for it in items {
            track_watermark(&mut res, it.item_updated_at.as_deref());
            match existing.remove(&it.external_id) {
                None => {
                    let inserted = insert_item(
                        &tx,
                        source_id,
                        run_id,
                        it,
                        &now,
                        &mut res,
                        store_media,
                        max_media_bytes,
                    )?;
                    existing.insert(it.external_id.clone(), inserted);
                }
                Some(ex) => {
                    let updated = update_item(
                        &tx,
                        run_id,
                        &ex,
                        it,
                        &now,
                        &mut res,
                        store_media,
                        max_media_bytes,
                    )?;
                    existing.insert(it.external_id.clone(), updated);
                }
            }
        }
        tx.commit()
            .map_err(|e| storage_err("failed to commit upsert_items transaction", e))?;
        Ok(res)
    }

    fn soft_delete_missing(
        &mut self,
        source_id: i64,
        live_ids: &HashSet<String>,
        run_id: i64,
        tag: Option<&str>,
    ) -> Result<u64, DbsError> {
        let now = self.now();
        let mut count: u64 = 0;
        let tx = self
            .conn
            .transaction()
            .map_err(|e| storage_err("failed to begin soft_delete_missing transaction", e))?;

        tx.execute(
            "CREATE TEMP TABLE IF NOT EXISTS _sweep_live(external_id TEXT PRIMARY KEY) WITHOUT ROWID",
            [],
        )
        .map_err(|e| storage_err("failed to create sweep temp table", e))?;
        tx.execute("DELETE FROM _sweep_live", [])
            .map_err(|e| storage_err("failed to clear sweep temp table", e))?;
        let live_ids_vec: Vec<&str> = live_ids.iter().map(String::as_str).collect();
        for chunk in live_ids_vec.chunks(CHUNK_SIZE) {
            let placeholders = vec!["(?)"; chunk.len()].join(",");
            let sql =
                format!("INSERT OR IGNORE INTO _sweep_live(external_id) VALUES {placeholders}");
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            tx.execute(&sql, params.as_slice())
                .map_err(|e| storage_err("failed to populate sweep temp table", e))?;
        }

        let mut victims_sql = "SELECT id, revision, content_hash, raw_json, title \
             FROM items WHERE source_id=?1 AND deleted=0 \
             AND external_id NOT IN (SELECT external_id FROM _sweep_live)"
            .to_string();
        if tag.is_some() {
            victims_sql.push_str(TAG_FILTER);
        }
        struct Victim {
            id: i64,
            revision: i64,
            content_hash: String,
            raw_json: String,
            title: Option<String>,
        }
        let victims: Vec<Victim> = {
            let mut stmt = tx
                .prepare(&victims_sql)
                .map_err(|e| storage_err("failed to prepare sweep victim query", e))?;
            let row_fn = |row: &rusqlite::Row<'_>| {
                Ok(Victim {
                    id: row.get(0)?,
                    revision: row.get(1)?,
                    content_hash: row.get(2)?,
                    raw_json: row.get(3)?,
                    title: row.get(4)?,
                })
            };
            let rows = if let Some(t) = tag {
                stmt.query_map(params![source_id, t], row_fn)
            } else {
                stmt.query_map(params![source_id], row_fn)
            }
            .map_err(|e| storage_err("failed to list sweep victims", e))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| storage_err("failed to read sweep victim row", e))?
        };

        for v in &victims {
            let new_rev = v.revision + 1;
            tx.execute(
                "UPDATE items SET deleted=1, deleted_at=?1, revision=?2, \
                 last_changed_at=?1, observed_run_id=?3 WHERE id=?4",
                params![now, new_rev, run_id, v.id],
            )
            .map_err(|e| storage_err("failed to soft-delete item", e))?;
            tx.execute(
                "INSERT INTO item_revisions(
                     item_id, revision, content_hash, raw_json, title,
                     captured_at, captured_run_id, change_kind)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'deleted')",
                params![
                    v.id,
                    new_rev,
                    v.content_hash,
                    v.raw_json,
                    v.title,
                    now,
                    run_id
                ],
            )
            .map_err(|e| storage_err("failed to write sweep deletion revision", e))?;
            count += 1;
        }
        tx.execute("DELETE FROM _sweep_live", [])
            .map_err(|e| storage_err("failed to clear sweep temp table", e))?;
        tx.commit()
            .map_err(|e| storage_err("failed to commit soft_delete_missing transaction", e))?;
        Ok(count)
    }

    fn live_external_ids(
        &self,
        source_id: i64,
        tag: Option<&str>,
    ) -> Result<HashSet<String>, DbsError> {
        let mut sql = "SELECT external_id FROM items WHERE source_id=?1 AND deleted=0".to_string();
        if tag.is_some() {
            sql.push_str(TAG_FILTER);
        }
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| storage_err("failed to prepare live_external_ids", e))?;
        let row_fn = |row: &rusqlite::Row<'_>| row.get::<_, String>(0);
        let rows = if let Some(t) = tag {
            stmt.query_map(params![source_id, t], row_fn)
        } else {
            stmt.query_map(params![source_id], row_fn)
        }
        .map_err(|e| storage_err("failed to list live external ids", e))?;
        rows.collect::<Result<HashSet<_>, _>>()
            .map_err(|e| storage_err("failed to read live external id row", e))
    }

    // -- cursor / state ---------------------------------------------------------

    fn save_cursor(
        &mut self,
        source_id: i64,
        cursor: Option<&Cursor>,
        watermark: Option<&str>,
        run_id: i64,
    ) -> Result<(), DbsError> {
        let now = self.now();
        let cursor_json = cursor
            .map(|c| serde_json::to_string(&c.value))
            .transpose()
            .map_err(|e| DbsError::Storage(format!("failed to encode cursor: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO sync_state(source_id, cursor_json, watermark, run_count, updated_at, updated_run_id)
                 VALUES (?1, ?2, ?3, COALESCE((SELECT run_count FROM sync_state WHERE source_id=?1), 0), ?4, ?5)
                 ON CONFLICT(source_id) DO UPDATE SET
                     cursor_json=excluded.cursor_json,
                     watermark=CASE
                         WHEN excluded.watermark IS NULL THEN sync_state.watermark
                         WHEN sync_state.watermark IS NULL THEN excluded.watermark
                         WHEN excluded.watermark > sync_state.watermark THEN excluded.watermark
                         ELSE sync_state.watermark END,
                     updated_at=excluded.updated_at,
                     updated_run_id=excluded.updated_run_id",
                params![source_id, cursor_json, watermark, now, run_id],
            )
            .map_err(|e| storage_err("failed to save cursor", e))?;
        Ok(())
    }

    fn load_cursor(
        &self,
        source_id: i64,
    ) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT cursor_json, watermark FROM sync_state WHERE source_id=?1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| storage_err("failed to load cursor", e))?;
        let Some((cursor_json, watermark)) = row else {
            return Ok((None, None));
        };
        let cursor = cursor_json
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()
            .map_err(|e| DbsError::Storage(format!("failed to decode cursor: {e}")))?
            .map(|value| Cursor { value });
        Ok((cursor, parse_iso(watermark.as_deref())))
    }

    fn get_run_count(&self, source_id: i64) -> Result<u64, DbsError> {
        let count: Option<i64> = self
            .conn
            .query_row(
                "SELECT run_count FROM sync_state WHERE source_id=?1",
                params![source_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| storage_err("failed to read run_count", e))?;
        Ok(count.unwrap_or(0) as u64)
    }

    fn increment_run_count(&mut self, source_id: i64) -> Result<(), DbsError> {
        let now = self.now();
        self.conn
            .execute(
                "INSERT INTO sync_state(source_id, run_count, updated_at)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT(source_id) DO UPDATE SET run_count = sync_state.run_count + 1",
                params![source_id, now],
            )
            .map_err(|e| storage_err("failed to increment run_count", e))?;
        Ok(())
    }

    // -- locking ----------------------------------------------------------------

    fn acquire_lock(&mut self, source_id: i64, run_id: i64) -> Result<bool, DbsError> {
        let now = self.now();
        match self.conn.execute(
            "INSERT INTO source_locks(source_id, run_id, acquired_at) VALUES (?1, ?2, ?3)",
            params![source_id, run_id, now],
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(e) => Err(storage_err("failed to acquire lock", e)),
        }
    }

    fn release_lock(&mut self, source_id: i64) -> Result<(), DbsError> {
        self.conn
            .execute(
                "DELETE FROM source_locks WHERE source_id=?1",
                params![source_id],
            )
            .map_err(|e| storage_err("failed to release lock", e))?;
        Ok(())
    }

    // -- export / stats -----------------------------------------------------------

    fn iter_items<'a>(
        &'a self,
        query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
        let (where_clause, params) = build_filter(query);
        let sql = format!(
            "SELECT i.*, s.name AS source_name, s.type AS source_type \
             FROM items i JOIN sources s ON s.id = i.source_id \
             WHERE {where_clause} ORDER BY s.name, i.item_created_at, i.external_id"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| storage_err("failed to prepare iter_items", e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), row_to_item)
            .map_err(|e| storage_err("failed to run iter_items", e))?;
        let items: Vec<ItemRow> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read iter_items row", e))?;
        Ok(Box::new(items.into_iter()))
    }

    fn iter_revisions<'a>(
        &'a self,
        query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
        let (where_clause, params) = build_filter(query);
        let sql = format!(
            "SELECT s.name AS source_name, s.type AS source_type, i.external_id, \
             i.item_kind, rv.revision, rv.content_hash, rv.change_kind, \
             rv.captured_at, rv.title, rv.raw_json \
             FROM item_revisions rv \
             JOIN items i ON i.id = rv.item_id \
             JOIN sources s ON s.id = i.source_id \
             WHERE {where_clause} ORDER BY s.name, i.external_id, rv.revision"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| storage_err("failed to prepare iter_revisions", e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), row_to_revision)
            .map_err(|e| storage_err("failed to run iter_revisions", e))?;
        let items: Vec<ItemRow> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read iter_revisions row", e))?;
        Ok(Box::new(items.into_iter()))
    }

    fn iter_media_blobs<'a>(
        &'a self,
        query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
        let (where_clause, params) = build_filter(query);
        let sql = format!(
            "SELECT s.name AS source_name, i.external_id, m.filename, m.kind, \
             m.mime, m.sha256, m.byte_size, m.data \
             FROM media m JOIN items i ON i.id = m.item_id JOIN sources s ON s.id = i.source_id \
             WHERE {where_clause} AND m.data IS NOT NULL ORDER BY s.name, i.external_id"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| storage_err("failed to prepare iter_media_blobs", e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), row_to_media_blob)
            .map_err(|e| storage_err("failed to run iter_media_blobs", e))?;
        let items: Vec<ItemRow> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read iter_media_blobs row", e))?;
        Ok(Box::new(items.into_iter()))
    }

    fn item_counts(&self, source_id: i64) -> Result<(u64, u64, u64), DbsError> {
        let (total, deleted): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*) AS total, \
                 SUM(CASE WHEN deleted=1 THEN 1 ELSE 0 END) AS deleted \
                 FROM items WHERE source_id=?1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
            )
            .map_err(|e| storage_err("failed to count items", e))?;
        let total = total.max(0) as u64;
        let deleted = deleted.max(0) as u64;
        Ok((total, total.saturating_sub(deleted), deleted))
    }

    fn browse_items(
        &self,
        query: &ExportQuery,
        text: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<ItemRow>, u64), DbsError> {
        // Text search: FTS5 when available (all-words, case-insensitive,
        // final-token prefix so search-as-you-type works), falling back
        // to the original LIKE substring scan — both when SQLite lacks
        // the FTS5 module and when a pathological query string trips
        // MATCH's parser. Mirrors the reference's `attempts` list.
        let (base_where, base_params) = build_filter(query);
        let mut attempts: Vec<(String, Vec<SqlValue>)> = Vec::new();
        if let Some(t) = text {
            if self.fts_enabled {
                let mut p = base_params.clone();
                p.push(SqlValue::Text(fts_match_query(t)));
                attempts.push((
                    format!(
                        "{base_where} AND i.id IN \
                         (SELECT rowid FROM items_fts WHERE items_fts MATCH ?)"
                    ),
                    p,
                ));
            }
        }
        if let Some(t) = text {
            let like = like_pattern(t);
            let mut p = base_params.clone();
            p.push(SqlValue::Text(like.clone()));
            p.push(SqlValue::Text(like));
            attempts.push((
                format!(
                    "{base_where} AND (i.title LIKE ? ESCAPE '\\' OR i.body LIKE ? ESCAPE '\\')"
                ),
                p,
            ));
        } else {
            attempts.push((base_where, base_params));
        }

        let mut last_err = None;
        for (where_clause, params) in attempts {
            match self.try_browse_items(&where_clause, params, limit, offset) {
                Ok(result) => return Ok(result),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            DbsError::Storage("browse_items produced no query attempts".to_string())
        }))
    }

    fn get_item(&self, item_id: i64) -> Result<Option<ItemRow>, DbsError> {
        let row: Option<ItemRow> = self
            .conn
            .query_row(
                "SELECT i.*, s.name AS source_name, s.type AS source_type \
                 FROM items i JOIN sources s ON s.id = i.source_id WHERE i.id=?1",
                params![item_id],
                row_to_item,
            )
            .optional()
            .map_err(|e| storage_err("failed to read item", e))?;
        let Some(mut out) = row else {
            return Ok(None);
        };
        out.insert("id".to_string(), Value::from(item_id));
        out.insert("media".to_string(), self.media_for_item(item_id)?);
        Ok(Some(out))
    }

    fn get_media_blob(&self, media_id: i64) -> Result<Option<ItemRow>, DbsError> {
        type MediaBlobRow = (i64, i64, Option<String>, Option<String>, Vec<u8>);
        let row: Option<MediaBlobRow> = self
            .conn
            .query_row(
                "SELECT id, item_id, filename, mime, data FROM media WHERE id=?1 AND data IS NOT NULL",
                params![media_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()
            .map_err(|e| storage_err("failed to read media blob", e))?;
        let Some((id, item_id, filename, mime, data)) = row else {
            return Ok(None);
        };
        let mut out = ItemRow::new();
        out.insert("id".to_string(), Value::from(id));
        out.insert("item_id".to_string(), Value::from(item_id));
        out.insert("filename".to_string(), opt_string(filename));
        out.insert("mime".to_string(), opt_string(mime));
        out.insert(
            "data".to_string(),
            serde_json::to_value(&data).unwrap_or(Value::Null),
        );
        Ok(Some(out))
    }

    fn metrics(&self) -> Result<ItemRow, DbsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.name AS source, i.item_kind AS kind, COUNT(*) AS total, \
                 SUM(CASE WHEN i.deleted=0 THEN 1 ELSE 0 END) AS live \
                 FROM items i JOIN sources s ON s.id = i.source_id \
                 GROUP BY s.name, i.item_kind ORDER BY s.name, i.item_kind",
            )
            .map_err(|e| storage_err("failed to prepare metrics", e))?;
        let by_source_kind: Vec<Value> = stmt
            .query_map([], |row| {
                let total: i64 = row.get("total")?;
                let live: i64 = row.get::<_, Option<i64>>("live")?.unwrap_or(0);
                Ok(serde_json::json!({
                    "source": row.get::<_, String>("source")?,
                    "kind": row.get::<_, String>("kind")?,
                    "total": total,
                    "live": live,
                    "deleted": total - live,
                }))
            })
            .map_err(|e| storage_err("failed to run metrics", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read metrics row", e))?;

        let revision_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM item_revisions", [], |row| row.get(0))
            .map_err(|e| storage_err("failed to count revisions", e))?;
        let (media_count, media_bytes): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*) AS n, COALESCE(SUM(byte_size), 0) AS bytes \
                 FROM media WHERE data IS NOT NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| storage_err("failed to count media", e))?;

        let mut out = ItemRow::new();
        out.insert("by_source_kind".to_string(), Value::Array(by_source_kind));
        out.insert("revision_count".to_string(), Value::from(revision_count));
        out.insert("media_count".to_string(), Value::from(media_count));
        out.insert("media_bytes".to_string(), Value::from(media_bytes));
        Ok(out)
    }

    fn integrity_check(&self) -> Result<String, DbsError> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .map_err(|e| storage_err("failed to run integrity_check", e))
    }

    // -- maintenance ----------------------------------------------------------------

    fn maintain(&mut self, vacuum: bool) -> Result<ItemRow, DbsError> {
        let size_before = self.file_size();
        let wal_ok: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(|e| storage_err("failed to checkpoint WAL", e))?;
        self.conn
            .execute("PRAGMA optimize", [])
            .map_err(|e| storage_err("failed to optimize", e))?;
        if vacuum {
            self.conn
                .execute("VACUUM", [])
                .map_err(|e| storage_err("failed to vacuum", e))?;
        }
        let size_after = self.file_size();

        let mut out = ItemRow::new();
        out.insert("path".to_string(), Value::from(self.path.clone()));
        out.insert("wal_checkpointed".to_string(), Value::from(wal_ok == 0));
        out.insert("optimized".to_string(), Value::from(true));
        out.insert("vacuumed".to_string(), Value::from(vacuum));
        out.insert("size_before".to_string(), Value::from(size_before));
        out.insert("size_after".to_string(), Value::from(size_after));
        Ok(out)
    }

    fn prune_revisions(&mut self, source_id: i64, keep: u32) -> Result<u64, DbsError> {
        if keep == 0 {
            return Ok(0);
        }
        let affected = self
            .conn
            .execute(
                "DELETE FROM item_revisions WHERE id IN (
                     SELECT rv.id FROM item_revisions rv
                     JOIN items i ON i.id = rv.item_id
                     WHERE i.source_id = ?1
                       AND rv.id NOT IN (
                         SELECT rv2.id FROM item_revisions rv2
                         WHERE rv2.item_id = rv.item_id
                         ORDER BY rv2.revision DESC LIMIT ?2
                       )
                 )",
                params![source_id, keep],
            )
            .map_err(|e| storage_err("failed to prune revisions", e))?;
        Ok(affected as u64)
    }

    fn vacuum_into(&self, dest: &Path) -> Result<u64, DbsError> {
        if dest.exists() {
            return Err(DbsError::Storage(format!(
                "snapshot target already exists: {}",
                dest.display()
            )));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DbsError::Storage(format!("failed to create snapshot parent directory: {e}"))
            })?;
        }
        let dest_str = dest.to_str().ok_or_else(|| {
            DbsError::Storage("snapshot destination path is not valid UTF-8".to_string())
        })?;
        self.conn
            .execute("VACUUM INTO ?1", params![dest_str])
            .map_err(|e| storage_err("failed to vacuum into snapshot", e))?;
        std::fs::metadata(dest)
            .map(|m| m.len())
            .map_err(|e| DbsError::Storage(format!("failed to stat snapshot: {e}")))
    }
}

impl SqliteStorage {
    /// Milliseconds from the run's `started_at` to `finished_at`, or
    /// `None` if the start is missing/unparseable — mirrors the
    /// reference's `_run_duration_ms` (derived from stored timestamps so
    /// it always agrees with them, rather than an independently-tracked
    /// wall-clock duration).
    fn run_duration_ms(&self, run_id: i64, finished_at: &str) -> Result<Option<i64>, DbsError> {
        let started_at: Option<String> = self
            .conn
            .query_row(
                "SELECT started_at FROM sync_runs WHERE id=?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| storage_err("failed to read run started_at", e))?;
        let (Some(started), Some(finished)) = (
            parse_iso(started_at.as_deref()),
            parse_iso(Some(finished_at)),
        ) else {
            return Ok(None);
        };
        Ok(Some((finished - started).num_milliseconds().max(0)))
    }

    /// Looks up existing `items` rows for a batch of external ids, chunked
    /// to stay under SQLite's bound-variable limit — mirrors the
    /// reference's `_existing_index`.
    fn existing_index(
        &self,
        source_id: i64,
        external_ids: &[&str],
    ) -> Result<HashMap<String, ExistingRow>, DbsError> {
        let mut index = HashMap::new();
        for chunk in external_ids.chunks(CHUNK_SIZE) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, external_id, content_hash, revision, deleted \
                 FROM items WHERE source_id=? AND external_id IN ({placeholders})"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .map_err(|e| storage_err("failed to prepare existing_index query", e))?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
            bound.push(&source_id);
            for id in chunk {
                bound.push(id);
            }
            let rows = stmt
                .query_map(bound.as_slice(), |row| {
                    let external_id: String = row.get(1)?;
                    Ok((
                        external_id,
                        ExistingRow {
                            id: row.get(0)?,
                            content_hash: row.get(2)?,
                            revision: row.get(3)?,
                            deleted: row.get::<_, i64>(4)? != 0,
                        },
                    ))
                })
                .map_err(|e| storage_err("failed to query existing items", e))?;
            for row in rows {
                let (external_id, existing) =
                    row.map_err(|e| storage_err("failed to read existing item row", e))?;
                index.insert(external_id, existing);
            }
        }
        Ok(index)
    }

    /// All media rows for one item (metadata only — `has_data` reports
    /// whether bytes were archived, without loading them). Mirrors the
    /// reference's `_media_for_item`.
    fn media_for_item(&self, item_id: i64) -> Result<Value, DbsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, url, kind, filename, mime, sha256, byte_size, local_path, \
                 (data IS NOT NULL) AS has_data \
                 FROM media WHERE item_id=?1 ORDER BY id",
            )
            .map_err(|e| storage_err("failed to prepare media_for_item", e))?;
        let rows = stmt
            .query_map(params![item_id], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, i64>("id")?,
                    "url": row.get::<_, String>("url")?,
                    "kind": row.get::<_, String>("kind")?,
                    "filename": row.get::<_, Option<String>>("filename")?,
                    "mime": row.get::<_, Option<String>>("mime")?,
                    "sha256": row.get::<_, Option<String>>("sha256")?,
                    "byte_size": row.get::<_, Option<i64>>("byte_size")?,
                    "local_path": row.get::<_, Option<String>>("local_path")?,
                    "has_data": row.get::<_, i64>("has_data")? != 0,
                }))
            })
            .map_err(|e| storage_err("failed to query media_for_item", e))?;
        let items: Vec<Value> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read media_for_item row", e))?;
        Ok(Value::Array(items))
    }

    /// The database file's size in bytes, or `0` for an in-memory database
    /// or one that no longer exists on disk — mirrors the reference's
    /// `_file_size` (which also swallows the `:memory:` case as an error
    /// it treats as "no size").
    fn file_size(&self) -> i64 {
        std::fs::metadata(&self.path)
            .map(|m| m.len() as i64)
            .unwrap_or(0)
    }

    /// Runs one `browse_items` "attempt" (a fully-built `where_clause` +
    /// `params`, from either the FTS5 or `LIKE` search path, or no text
    /// search at all) — count then page. A single unit so
    /// `browse_items` can try the next attempt on any error.
    fn try_browse_items(
        &self,
        where_clause: &str,
        params: Vec<SqlValue>,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<ItemRow>, u64), DbsError> {
        let total: i64 = self
            .conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM items i JOIN sources s ON s.id = i.source_id WHERE {where_clause}"
                ),
                rusqlite::params_from_iter(params.clone()),
                |row| row.get(0),
            )
            .map_err(|e| storage_err("failed to count browse_items", e))?;

        let sql = format!(
            "SELECT i.*, s.name AS source_name, s.type AS source_type, \
             (SELECT COUNT(*) FROM media m WHERE m.item_id = i.id) AS media_count, \
             COALESCE(\
                 (SELECT m.url FROM media m WHERE m.item_id = i.id AND m.kind = 'image' \
                  ORDER BY m.id LIMIT 1), \
                 CASE WHEN json_extract(i.raw_json, '$.videoLink') LIKE '%youtu%' \
                       OR json_extract(i.raw_json, '$.videoLink') LIKE '%loom.com%' \
                       OR json_extract(i.raw_json, '$.videoLink') LIKE '%vimeo.com%' \
                      THEN json_extract(i.raw_json, '$.videoLink') END\
             ) AS thumb_url \
             FROM items i JOIN sources s ON s.id = i.source_id \
             WHERE {where_clause} ORDER BY i.item_created_at DESC, i.id DESC LIMIT ? OFFSET ?"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| storage_err("failed to prepare browse_items", e))?;
        let mut all_params = params;
        all_params.push(SqlValue::Integer(limit.max(1) as i64));
        all_params.push(SqlValue::Integer(offset as i64));
        let rows = stmt
            .query_map(rusqlite::params_from_iter(all_params), row_to_browse_item)
            .map_err(|e| storage_err("failed to run browse_items", e))?;
        let items: Vec<ItemRow> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| storage_err("failed to read browse_items row", e))?;
        Ok((items, total.max(0) as u64))
    }
}

fn row_to_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRecord> {
    Ok(SourceRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        type_: row.get("type")?,
        plugin_id: row.get("plugin_id")?,
        config_json: row.get("config_json")?,
        schema_version: row.get::<_, i64>("schema_version")? as u32,
        enabled: row.get::<_, i64>("enabled")? != 0,
        created_at: row.get("created_at")?,
    })
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    let mut out: ItemRow = HashMap::new();
    out.insert("id".to_string(), Value::from(row.get::<_, i64>("id")?));
    out.insert(
        "source_id".to_string(),
        Value::from(row.get::<_, i64>("source_id")?),
    );
    out.insert(
        "source_name".to_string(),
        Value::from(row.get::<_, String>("source_name")?),
    );
    out.insert(
        "plugin_id".to_string(),
        Value::from(row.get::<_, String>("plugin_id")?),
    );
    out.insert(
        "status".to_string(),
        Value::from(row.get::<_, String>("status")?),
    );
    out.insert(
        "mode".to_string(),
        Value::from(row.get::<_, String>("mode")?),
    );
    out.insert(
        "started_at".to_string(),
        Value::from(row.get::<_, String>("started_at")?),
    );
    out.insert(
        "finished_at".to_string(),
        opt_string(row.get::<_, Option<String>>("finished_at")?),
    );
    out.insert(
        "items_seen".to_string(),
        Value::from(row.get::<_, i64>("items_seen")?),
    );
    out.insert(
        "items_created".to_string(),
        Value::from(row.get::<_, i64>("items_created")?),
    );
    out.insert(
        "items_updated".to_string(),
        Value::from(row.get::<_, i64>("items_updated")?),
    );
    out.insert(
        "items_unchanged".to_string(),
        Value::from(row.get::<_, i64>("items_unchanged")?),
    );
    out.insert(
        "items_deleted".to_string(),
        Value::from(row.get::<_, i64>("items_deleted")?),
    );
    out.insert(
        "items_undeleted".to_string(),
        Value::from(row.get::<_, i64>("items_undeleted")?),
    );
    out.insert(
        "revisions".to_string(),
        Value::from(row.get::<_, i64>("revisions")?),
    );
    out.insert(
        "cursor_before".to_string(),
        opt_string(row.get::<_, Option<String>>("cursor_before")?),
    );
    out.insert(
        "cursor_after".to_string(),
        opt_string(row.get::<_, Option<String>>("cursor_after")?),
    );
    out.insert(
        "error".to_string(),
        opt_string(row.get::<_, Option<String>>("error")?),
    );
    let warnings_raw: Option<String> = row.get("warnings")?;
    let warnings = match warnings_raw {
        Some(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Array(Vec::new())),
        None => Value::Array(Vec::new()),
    };
    out.insert("warnings".to_string(), warnings);
    out.insert(
        "duration_ms".to_string(),
        match row.get::<_, Option<i64>>("duration_ms")? {
            Some(v) => Value::from(v),
            None => Value::Null,
        },
    );
    out.insert(
        "items_failed".to_string(),
        Value::from(row.get::<_, i64>("items_failed")?),
    );
    Ok(out)
}

fn opt_string(v: Option<String>) -> Value {
    match v {
        Some(s) => Value::from(s),
        None => Value::Null,
    }
}

/// Mirrors the reference's `_row_to_item` — always includes `raw` (this
/// port's `ExportQuery` has no `include_raw` toggle; see #11).
fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    let mut out = ItemRow::new();
    out.insert(
        "source".to_string(),
        Value::from(row.get::<_, String>("source_name")?),
    );
    out.insert(
        "type".to_string(),
        Value::from(row.get::<_, String>("source_type")?),
    );
    out.insert(
        "external_id".to_string(),
        Value::from(row.get::<_, String>("external_id")?),
    );
    out.insert(
        "item_kind".to_string(),
        Value::from(row.get::<_, String>("item_kind")?),
    );
    out.insert("title".to_string(), opt_string(row.get("title")?));
    out.insert("url".to_string(), opt_string(row.get("url")?));
    out.insert("body".to_string(), opt_string(row.get("body")?));
    let tags_json: String = row.get("tags_json")?;
    out.insert(
        "tags".to_string(),
        serde_json::from_str(&tags_json).unwrap_or(Value::Array(Vec::new())),
    );
    out.insert(
        "created_at".to_string(),
        opt_string(row.get("item_created_at")?),
    );
    out.insert(
        "updated_at".to_string(),
        opt_string(row.get("item_updated_at")?),
    );
    out.insert(
        "content_hash".to_string(),
        Value::from(row.get::<_, String>("content_hash")?),
    );
    out.insert(
        "revision".to_string(),
        Value::from(row.get::<_, i64>("revision")?),
    );
    out.insert(
        "first_seen_at".to_string(),
        Value::from(row.get::<_, String>("first_seen_at")?),
    );
    out.insert(
        "last_seen_at".to_string(),
        Value::from(row.get::<_, String>("last_seen_at")?),
    );
    out.insert(
        "last_changed_at".to_string(),
        Value::from(row.get::<_, String>("last_changed_at")?),
    );
    out.insert(
        "deleted".to_string(),
        Value::from(row.get::<_, i64>("deleted")? != 0),
    );
    out.insert("deleted_at".to_string(), opt_string(row.get("deleted_at")?));
    let raw_json: String = row.get("raw_json")?;
    out.insert(
        "raw".to_string(),
        serde_json::from_str(&raw_json).unwrap_or(Value::Null),
    );
    Ok(out)
}

/// Mirrors the reference's `_row_to_item` revision-row shape used by
/// `iter_revisions` — a lighter projection than `row_to_item`.
fn row_to_revision(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    let mut out = ItemRow::new();
    out.insert(
        "source".to_string(),
        Value::from(row.get::<_, String>("source_name")?),
    );
    out.insert(
        "type".to_string(),
        Value::from(row.get::<_, String>("source_type")?),
    );
    out.insert(
        "external_id".to_string(),
        Value::from(row.get::<_, String>("external_id")?),
    );
    out.insert(
        "item_kind".to_string(),
        Value::from(row.get::<_, String>("item_kind")?),
    );
    out.insert(
        "revision".to_string(),
        Value::from(row.get::<_, i64>("revision")?),
    );
    out.insert(
        "content_hash".to_string(),
        Value::from(row.get::<_, String>("content_hash")?),
    );
    out.insert(
        "change_kind".to_string(),
        Value::from(row.get::<_, String>("change_kind")?),
    );
    out.insert(
        "captured_at".to_string(),
        Value::from(row.get::<_, String>("captured_at")?),
    );
    out.insert("title".to_string(), opt_string(row.get("title")?));
    let raw_json: String = row.get("raw_json")?;
    out.insert(
        "raw".to_string(),
        serde_json::from_str(&raw_json).unwrap_or(Value::Null),
    );
    Ok(out)
}

/// Mirrors the reference's `iter_media_blobs` row shape. `data` is
/// encoded as a JSON array of byte values — see the module doc-comment
/// on why (no binary variant in `ItemRow`, no new dependency for it).
fn row_to_media_blob(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    let mut out = ItemRow::new();
    out.insert(
        "source".to_string(),
        Value::from(row.get::<_, String>("source_name")?),
    );
    out.insert(
        "external_id".to_string(),
        Value::from(row.get::<_, String>("external_id")?),
    );
    out.insert("filename".to_string(), opt_string(row.get("filename")?));
    out.insert(
        "kind".to_string(),
        Value::from(row.get::<_, String>("kind")?),
    );
    out.insert("mime".to_string(), opt_string(row.get("mime")?));
    out.insert("sha256".to_string(), opt_string(row.get("sha256")?));
    out.insert(
        "byte_size".to_string(),
        match row.get::<_, Option<i64>>("byte_size")? {
            Some(v) => Value::from(v),
            None => Value::Null,
        },
    );
    let data: Vec<u8> = row.get("data")?;
    out.insert(
        "data".to_string(),
        serde_json::to_value(&data).unwrap_or(Value::Null),
    );
    Ok(out)
}

/// Lighter item shape for the paginated browse listing (no raw payload)
/// — mirrors the reference's `_row_to_browse_item`, including its
/// video-link thumbnail fallback (see `thumb_url`'s `COALESCE` in
/// `try_browse_items`, #48).
fn row_to_browse_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    let mut out = ItemRow::new();
    out.insert("id".to_string(), Value::from(row.get::<_, i64>("id")?));
    out.insert(
        "source".to_string(),
        Value::from(row.get::<_, String>("source_name")?),
    );
    out.insert(
        "type".to_string(),
        Value::from(row.get::<_, String>("source_type")?),
    );
    out.insert(
        "external_id".to_string(),
        Value::from(row.get::<_, String>("external_id")?),
    );
    out.insert(
        "item_kind".to_string(),
        Value::from(row.get::<_, String>("item_kind")?),
    );
    out.insert("title".to_string(), opt_string(row.get("title")?));
    out.insert("url".to_string(), opt_string(row.get("url")?));
    out.insert(
        "created_at".to_string(),
        opt_string(row.get("item_created_at")?),
    );
    out.insert(
        "updated_at".to_string(),
        opt_string(row.get("item_updated_at")?),
    );
    out.insert(
        "revision".to_string(),
        Value::from(row.get::<_, i64>("revision")?),
    );
    out.insert(
        "deleted".to_string(),
        Value::from(row.get::<_, i64>("deleted")? != 0),
    );
    out.insert("deleted_at".to_string(), opt_string(row.get("deleted_at")?));
    out.insert(
        "media_count".to_string(),
        Value::from(row.get::<_, i64>("media_count")?),
    );
    let tags_json: String = row.get("tags_json")?;
    out.insert(
        "tags".to_string(),
        serde_json::from_str(&tags_json).unwrap_or(Value::Array(Vec::new())),
    );
    out.insert("thumbnail".to_string(), opt_string(row.get("thumb_url")?));
    Ok(out)
}

fn track_watermark(res: &mut BatchResult, updated_at: Option<&str>) {
    let Some(updated_at) = updated_at else {
        return;
    };
    let advance = match &res.max_updated_at {
        Some(current) => updated_at > current.as_str(),
        None => true,
    };
    if advance {
        res.max_updated_at = Some(updated_at.to_string());
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_item(
    tx: &rusqlite::Transaction<'_>,
    source_id: i64,
    run_id: i64,
    it: &PreparedItem,
    now: &str,
    res: &mut BatchResult,
    store_media: bool,
    max_media_bytes: u64,
) -> Result<ExistingRow, DbsError> {
    let deleted = it.deleted;
    let change_kind = if deleted { "deleted" } else { "created" };
    let tags_json = serde_json::to_string(&it.tags)
        .map_err(|e| DbsError::Storage(format!("failed to encode tags: {e}")))?;
    tx.execute(
        "INSERT INTO items(
             source_id, external_id, item_kind, title, url, body, tags_json,
             item_created_at, item_updated_at, content_hash, raw_json, revision,
             first_seen_at, last_seen_at, last_changed_at, observed_run_id,
             deleted, deleted_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12,?12,?12,?13,?14,?15)",
        params![
            source_id,
            it.external_id,
            it.item_kind,
            it.title,
            it.url,
            it.body,
            tags_json,
            it.item_created_at,
            it.item_updated_at,
            it.content_hash,
            it.raw_json,
            now,
            run_id,
            deleted as i64,
            if deleted { Some(now) } else { None },
        ],
    )
    .map_err(|e| storage_err("failed to insert item", e))?;
    let item_id = tx.last_insert_rowid();
    insert_revision(tx, item_id, 1, it, now, run_id, change_kind)?;
    replace_media(tx, item_id, it, now, store_media, max_media_bytes)?;
    res.revisions += 1;
    if deleted {
        res.deleted += 1;
    } else {
        res.created += 1;
    }
    Ok(ExistingRow {
        id: item_id,
        content_hash: it.content_hash.clone(),
        revision: 1,
        deleted,
    })
}

#[allow(clippy::too_many_arguments)]
fn update_item(
    tx: &rusqlite::Transaction<'_>,
    run_id: i64,
    ex: &ExistingRow,
    it: &PreparedItem,
    now: &str,
    res: &mut BatchResult,
    store_media: bool,
    max_media_bytes: u64,
) -> Result<ExistingRow, DbsError> {
    let was_deleted = ex.deleted;
    let hash_changed = ex.content_hash != it.content_hash;
    let mut new_rev = ex.revision;
    let mut deleted = was_deleted;
    let mut content_hash = ex.content_hash.clone();

    if it.deleted && !was_deleted {
        new_rev += 1;
        write_full_update(tx, ex.id, new_rev, it, now, run_id, true)?;
        insert_revision(tx, ex.id, new_rev, it, now, run_id, "deleted")?;
        replace_media(tx, ex.id, it, now, store_media, max_media_bytes)?;
        res.deleted += 1;
        res.revisions += 1;
        deleted = true;
        content_hash = it.content_hash.clone();
    } else if was_deleted && !it.deleted {
        new_rev += 1;
        write_full_update(tx, ex.id, new_rev, it, now, run_id, false)?;
        insert_revision(tx, ex.id, new_rev, it, now, run_id, "undeleted")?;
        replace_media(tx, ex.id, it, now, store_media, max_media_bytes)?;
        res.undeleted += 1;
        res.revisions += 1;
        deleted = false;
        content_hash = it.content_hash.clone();
    } else if hash_changed {
        // A still-deleted item whose payload changed stays deleted — a
        // native-deletes source may re-emit trash items with mutated
        // payloads, and an update must never resurrect them.
        new_rev += 1;
        write_full_update(tx, ex.id, new_rev, it, now, run_id, it.deleted)?;
        insert_revision(tx, ex.id, new_rev, it, now, run_id, "updated")?;
        replace_media(tx, ex.id, it, now, store_media, max_media_bytes)?;
        res.updated += 1;
        res.revisions += 1;
        deleted = it.deleted;
        content_hash = it.content_hash.clone();
    } else {
        tx.execute(
            "UPDATE items SET last_seen_at=?1, observed_run_id=?2 WHERE id=?3",
            params![now, run_id, ex.id],
        )
        .map_err(|e| storage_err("failed to bump item last_seen_at", e))?;
        res.unchanged += 1;
    }
    Ok(ExistingRow {
        id: ex.id,
        content_hash,
        revision: new_rev,
        deleted,
    })
}

fn write_full_update(
    tx: &rusqlite::Transaction<'_>,
    item_id: i64,
    new_rev: i64,
    it: &PreparedItem,
    now: &str,
    run_id: i64,
    deleted: bool,
) -> Result<(), DbsError> {
    let tags_json = serde_json::to_string(&it.tags)
        .map_err(|e| DbsError::Storage(format!("failed to encode tags: {e}")))?;
    tx.execute(
        "UPDATE items SET
             item_kind=?1, title=?2, url=?3, body=?4, tags_json=?5,
             item_created_at=?6, item_updated_at=?7, content_hash=?8, raw_json=?9,
             revision=?10, last_seen_at=?11, last_changed_at=?11, observed_run_id=?12,
             deleted=?13,
             deleted_at=CASE WHEN ?13 THEN COALESCE(deleted_at, ?11) ELSE NULL END
         WHERE id=?14",
        params![
            it.item_kind,
            it.title,
            it.url,
            it.body,
            tags_json,
            it.item_created_at,
            it.item_updated_at,
            it.content_hash,
            it.raw_json,
            new_rev,
            now,
            run_id,
            deleted as i64,
            item_id,
        ],
    )
    .map_err(|e| storage_err("failed to write item update", e))?;
    Ok(())
}

fn insert_revision(
    tx: &rusqlite::Transaction<'_>,
    item_id: i64,
    revision: i64,
    it: &PreparedItem,
    now: &str,
    run_id: i64,
    kind: &str,
) -> Result<(), DbsError> {
    tx.execute(
        "INSERT INTO item_revisions(
             item_id, revision, content_hash, raw_json, title,
             captured_at, captured_run_id, change_kind)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            item_id,
            revision,
            it.content_hash,
            it.raw_json,
            it.title,
            now,
            run_id,
            kind,
        ],
    )
    .map_err(|e| storage_err("failed to insert item revision", e))?;
    Ok(())
}

/// Deletes then re-inserts every media row for `item_id` — a no-op when
/// `it.media` is empty, matching the reference (items that never declare
/// media never touch the `media` table). `OR REPLACE` (not `OR IGNORE`):
/// the same item listing the same URL twice with differing metadata keeps
/// the latest rather than dropping it.
fn replace_media(
    tx: &rusqlite::Transaction<'_>,
    item_id: i64,
    it: &PreparedItem,
    now: &str,
    store_media: bool,
    max_media_bytes: u64,
) -> Result<(), DbsError> {
    if it.media.is_empty() {
        return Ok(());
    }
    tx.execute("DELETE FROM media WHERE item_id=?1", params![item_id])
        .map_err(|e| storage_err("failed to clear existing media", e))?;
    for raw in &it.media {
        let m: MediaRef = serde_json::from_value(raw.clone())
            .map_err(|e| DbsError::Storage(format!("invalid media entry: {e}")))?;
        let (data, byte_size, sha, local_path) = if store_media {
            if let Some(supplied) = &m.data {
                let (d, size, s) = resolve_supplied_media(supplied, max_media_bytes);
                (d, size, s, None)
            } else {
                resolve_local_media(&m.url, max_media_bytes)
            }
        } else {
            (None, None, None, None)
        };
        let fetched_at = data.as_ref().map(|_| now.to_string());
        tx.execute(
            "INSERT OR REPLACE INTO media
                 (item_id, url, kind, filename, mime, local_path, sha256, fetched_at, data, byte_size)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                item_id,
                m.url,
                m.kind,
                m.filename,
                m.mime,
                local_path,
                sha,
                fetched_at,
                data,
                byte_size,
            ],
        )
        .map_err(|e| storage_err("failed to insert media row", e))?;
    }
    Ok(())
}

/// Loads a local-file media reference for inline storage. Only **local
/// files** are ingested — a bare URL is left as a reference (v1). A file
/// larger than `max_bytes` (when `>0`) is recorded by path + size, but
/// its bytes are not stored, so an opt-in archive can't be ballooned by
/// one huge asset. Returns `(data, byte_size, sha256, local_path)`.
fn resolve_local_media(
    url: &str,
    max_bytes: u64,
) -> (Option<Vec<u8>>, Option<i64>, Option<String>, Option<String>) {
    let expanded = crate::storage::sqlite::shellexpand_home(url);
    let path = std::path::Path::new(&expanded);
    let metadata = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return (None, None, None, None),
    };
    let size = metadata.len() as i64;
    let local_path = path.to_string_lossy().into_owned();
    if max_bytes > 0 && metadata.len() > max_bytes {
        return (None, Some(size), None, Some(local_path));
    }
    match std::fs::read(path) {
        Ok(data) => {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let sha = format!("{:x}", hasher.finalize());
            let len = data.len() as i64;
            (Some(data), Some(len), Some(sha), Some(local_path))
        }
        Err(_) => (None, Some(size), None, Some(local_path)),
    }
}

/// Accepts bytes a connector already fetched over HTTP (`MediaRef::data`).
/// Size-capped identically to [`resolve_local_media`] — over-cap bytes
/// are dropped but the size is still reported. Returns `(data,
/// byte_size, sha256)`.
fn resolve_supplied_media(
    data: &[u8],
    max_bytes: u64,
) -> (Option<Vec<u8>>, Option<i64>, Option<String>) {
    let size = data.len() as i64;
    if max_bytes > 0 && (data.len() as u64) > max_bytes {
        return (None, Some(size), None);
    }
    let mut hasher = Sha256::new();
    hasher.update(data);
    let sha = format!("{:x}", hasher.finalize());
    (Some(data.to_vec()), Some(size), Some(sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> SqliteStorage {
        SqliteStorage::open(":memory:").unwrap()
    }

    #[test]
    fn migrate_is_idempotent_after_open() {
        let mut storage = open();
        storage.migrate().unwrap();
        storage.migrate().unwrap();
    }

    #[test]
    fn upsert_source_creates_then_updates() {
        let mut storage = open();
        let created = storage
            .upsert_source("raindrop", "raindrop", "rusty_dbs:raindrop", "{}", 1)
            .unwrap();
        assert_eq!(created.name, "raindrop");
        assert!(created.enabled);

        let updated = storage
            .upsert_source("raindrop", "raindrop", "rusty_dbs:raindrop", "{\"x\":1}", 2)
            .unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.schema_version, 2);
        assert_eq!(updated.config_json, "{\"x\":1}");
    }

    #[test]
    fn get_source_returns_none_when_missing() {
        let storage = open();
        assert!(storage.get_source("missing").unwrap().is_none());
    }

    #[test]
    fn list_sources_is_sorted_by_name() {
        let mut storage = open();
        storage.upsert_source("z", "t", "p", "{}", 1).unwrap();
        storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let names: Vec<String> = storage
            .list_sources()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["a", "z"]);
    }

    #[test]
    fn delete_source_reports_whether_a_row_was_removed() {
        let mut storage = open();
        storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        assert!(storage.delete_source("a").unwrap());
        assert!(!storage.delete_source("a").unwrap());
    }

    #[test]
    fn begin_and_finish_run_round_trips_stats() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        assert!(run_id > 0);

        let stats = BatchResult {
            created: 2,
            updated: 1,
            ..Default::default()
        };
        storage
            .finish_run(
                run_id,
                "success",
                &stats,
                3,
                Some("cursor-after"),
                None,
                &["careful".to_string()],
            )
            .unwrap();

        let runs = storage.recent_runs(Some(source.id), 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], Value::from("success"));
        assert_eq!(runs[0]["items_created"], Value::from(2));
        assert_eq!(runs[0]["items_updated"], Value::from(1));
        assert_eq!(runs[0]["warnings"], Value::from(vec!["careful"]));
        assert_eq!(runs[0]["source_name"], Value::from("a"));
    }

    #[test]
    fn recent_runs_without_source_id_spans_all_sources() {
        let mut storage = open();
        let a = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let b = storage.upsert_source("b", "t", "p", "{}", 1).unwrap();
        storage.begin_run(a.id, "p", "incremental", None).unwrap();
        storage.begin_run(b.id, "p", "incremental", None).unwrap();
        assert_eq!(storage.recent_runs(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn reap_interrupted_runs_flips_running_to_interrupted_and_clears_locks() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        assert!(storage.acquire_lock(source.id, run_id).unwrap());

        let reaped = storage.reap_interrupted_runs().unwrap();
        assert_eq!(reaped, vec![run_id]);

        let runs = storage.recent_runs(Some(source.id), 10).unwrap();
        assert_eq!(runs[0]["status"], Value::from("interrupted"));

        // The lock was cleared, so a fresh run can acquire it.
        let new_run = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        assert!(storage.acquire_lock(source.id, new_run).unwrap());
    }

    #[test]
    fn reap_interrupted_runs_is_a_noop_with_nothing_running() {
        let mut storage = open();
        assert!(storage.reap_interrupted_runs().unwrap().is_empty());
    }

    #[test]
    fn save_and_load_cursor_round_trips() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();

        let cursor = Cursor {
            value: serde_json::json!({"page": 3}),
        };
        storage
            .save_cursor(
                source.id,
                Some(&cursor),
                Some("2026-01-01T00:00:00Z"),
                run_id,
            )
            .unwrap();

        let (loaded, watermark) = storage.load_cursor(source.id).unwrap();
        assert_eq!(loaded, Some(cursor));
        assert_eq!(watermark, parse_iso(Some("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn load_cursor_returns_none_for_an_unknown_source() {
        let storage = open();
        assert_eq!(storage.load_cursor(999).unwrap(), (None, None));
    }

    #[test]
    fn save_cursor_watermark_only_advances() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();

        storage
            .save_cursor(source.id, None, Some("2026-01-05T00:00:00Z"), run_id)
            .unwrap();
        storage
            .save_cursor(source.id, None, Some("2026-01-01T00:00:00Z"), run_id)
            .unwrap();

        let (_, watermark) = storage.load_cursor(source.id).unwrap();
        assert_eq!(watermark, parse_iso(Some("2026-01-05T00:00:00Z")));
    }

    #[test]
    fn run_count_starts_at_zero_and_increments() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        assert_eq!(storage.get_run_count(source.id).unwrap(), 0);
        storage.increment_run_count(source.id).unwrap();
        storage.increment_run_count(source.id).unwrap();
        assert_eq!(storage.get_run_count(source.id).unwrap(), 2);
    }

    #[test]
    fn acquire_lock_fails_when_already_held() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        assert!(storage.acquire_lock(source.id, run_id).unwrap());
        assert!(!storage.acquire_lock(source.id, run_id).unwrap());
        storage.release_lock(source.id).unwrap();
        assert!(storage.acquire_lock(source.id, run_id).unwrap());
    }

    #[test]
    fn integrity_check_reports_ok_on_a_fresh_database() {
        let storage = open();
        assert_eq!(storage.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn spawn_returns_none_for_an_in_memory_database() {
        let storage = open();
        assert!(storage.spawn().is_none());
    }

    #[test]
    fn spawn_opens_a_second_connection_to_a_file_backed_database() {
        let dir = std::env::temp_dir().join(format!(
            "rusty_dbs_sqlite_storage_test_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.sqlite3");
        let storage = SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        let worker = storage.spawn();
        assert!(worker.is_some());
        drop(worker);
        drop(storage);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn prepared(external_id: &str, hash: &str) -> PreparedItem {
        PreparedItem {
            external_id: external_id.to_string(),
            item_kind: "post".to_string(),
            title: Some("Title".to_string()),
            url: Some("https://example.com".to_string()),
            body: Some("Body".to_string()),
            tags: vec!["a".to_string(), "b".to_string()],
            item_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            item_updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            content_hash: hash.to_string(),
            raw_json: "{}".to_string(),
            deleted: false,
            media: Vec::new(),
        }
    }

    #[test]
    fn upsert_items_with_no_items_is_a_noop() {
        let mut storage = open();
        let result = storage.upsert_items(1, 1, &[], false, 0).unwrap();
        assert_eq!(result, BatchResult::default());
    }

    #[test]
    fn upsert_items_inserts_new_items_as_created() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let items = vec![prepared("e1", "h1"), prepared("e2", "h2")];
        let result = storage
            .upsert_items(source.id, run_id, &items, false, 0)
            .unwrap();
        assert_eq!(result.created, 2);
        assert_eq!(result.revisions, 2);
        assert_eq!(
            result.max_updated_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );

        let live = storage.live_external_ids(source.id, None).unwrap();
        assert_eq!(live.len(), 2);
        assert!(live.contains("e1"));
    }

    #[test]
    fn upsert_items_reports_unchanged_when_hash_is_the_same() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let item = prepared("e1", "h1");
        storage
            .upsert_items(source.id, run_id, std::slice::from_ref(&item), false, 0)
            .unwrap();
        let result = storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();
        assert_eq!(result.unchanged, 1);
        assert_eq!(result.created, 0);
    }

    #[test]
    fn upsert_items_reports_updated_when_hash_changes() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h1")], false, 0)
            .unwrap();
        let result = storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h2")], false, 0)
            .unwrap();
        assert_eq!(result.updated, 1);
        assert_eq!(result.revisions, 1);
    }

    #[test]
    fn upsert_items_marks_a_native_delete_then_can_undelete() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h1")], false, 0)
            .unwrap();

        let mut deleted_item = prepared("e1", "h1");
        deleted_item.deleted = true;
        let result = storage
            .upsert_items(source.id, run_id, &[deleted_item], false, 0)
            .unwrap();
        assert_eq!(result.deleted, 1);
        assert!(storage
            .live_external_ids(source.id, None)
            .unwrap()
            .is_empty());

        let result = storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h2")], false, 0)
            .unwrap();
        assert_eq!(result.undeleted, 1);
        assert!(storage
            .live_external_ids(source.id, None)
            .unwrap()
            .contains("e1"));
    }

    #[test]
    fn upsert_items_stores_local_file_media_when_store_media_is_set() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "rusty_dbs_sqlite_storage_media_test_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("thumb.png");
        std::fs::write(&file_path, b"fake-png-bytes").unwrap();

        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: file_path.to_string_lossy().into_owned(),
            kind: "image".to_string(),
            filename: Some("thumb.png".to_string()),
            mime: Some("image/png".to_string()),
            data: None,
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];

        storage
            .upsert_items(source.id, run_id, &[item], true, 0)
            .unwrap();

        let has_data: i64 = storage
            .conn
            .query_row("SELECT data IS NOT NULL FROM media", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has_data, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_items_skips_media_bytes_without_store_media() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();
        let has_data: i64 = storage
            .conn
            .query_row("SELECT data IS NOT NULL FROM media", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has_data, 0);
    }

    #[test]
    fn upsert_items_caps_supplied_media_bytes_at_max_media_bytes() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: Some(vec![0u8; 100]),
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], true, 10)
            .unwrap();
        let (has_data, byte_size): (i64, i64) = storage
            .conn
            .query_row("SELECT data IS NOT NULL, byte_size FROM media", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(has_data, 0);
        assert_eq!(byte_size, 100);
    }

    #[test]
    fn soft_delete_missing_removes_items_absent_from_live_ids() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        storage
            .upsert_items(
                source.id,
                run_id,
                &[prepared("e1", "h1"), prepared("e2", "h2")],
                false,
                0,
            )
            .unwrap();

        let live: HashSet<String> = ["e1".to_string()].into_iter().collect();
        let count = storage
            .soft_delete_missing(source.id, &live, run_id, None)
            .unwrap();
        assert_eq!(count, 1);

        let remaining = storage.live_external_ids(source.id, None).unwrap();
        assert_eq!(remaining, live);
    }

    #[test]
    fn soft_delete_missing_is_idempotent_on_a_second_pass() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h1")], false, 0)
            .unwrap();
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(
            storage
                .soft_delete_missing(source.id, &empty, run_id, None)
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .soft_delete_missing(source.id, &empty, run_id, None)
                .unwrap(),
            0
        );
    }

    #[test]
    fn live_external_ids_filters_by_tag_when_given() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut tagged = prepared("e1", "h1");
        tagged.tags = vec!["keep".to_string()];
        let untagged = prepared("e2", "h2");
        storage
            .upsert_items(source.id, run_id, &[tagged, untagged], false, 0)
            .unwrap();

        let all = storage.live_external_ids(source.id, None).unwrap();
        assert_eq!(all.len(), 2);
        let only_tagged = storage.live_external_ids(source.id, Some("keep")).unwrap();
        assert_eq!(only_tagged, ["e1".to_string()].into_iter().collect());
    }

    #[test]
    fn soft_delete_missing_with_tag_only_touches_tagged_items() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut tagged = prepared("e1", "h1");
        tagged.tags = vec!["sweep".to_string()];
        let untagged = prepared("e2", "h2");
        storage
            .upsert_items(source.id, run_id, &[tagged, untagged], false, 0)
            .unwrap();

        let empty: HashSet<String> = HashSet::new();
        let count = storage
            .soft_delete_missing(source.id, &empty, run_id, Some("sweep"))
            .unwrap();
        assert_eq!(count, 1);
        let remaining = storage.live_external_ids(source.id, None).unwrap();
        assert_eq!(remaining, ["e2".to_string()].into_iter().collect());
    }

    fn seeded_storage() -> (SqliteStorage, SourceRecord, i64) {
        let mut storage = open();
        let source = storage
            .upsert_source("a", "raindrop", "p", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        item.title = Some("Hello World".to_string());
        item.body = Some("a body about cats".to_string());
        storage
            .upsert_items(source.id, run_id, &[item, prepared("e2", "h2")], false, 0)
            .unwrap();
        (storage, source, run_id)
    }

    #[test]
    fn iter_items_returns_every_matching_item_with_raw() {
        let (storage, source, _) = seeded_storage();
        let rows: Vec<ItemRow> = storage
            .iter_items(&ExportQuery::default())
            .unwrap()
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["source"], Value::from("a"));
        assert!(rows.iter().all(|r| r.contains_key("raw")));

        let scoped = ExportQuery {
            source_id: Some(source.id),
            ..Default::default()
        };
        assert_eq!(storage.iter_items(&scoped).unwrap().count(), 2);
        let none = ExportQuery {
            source_id: Some(source.id + 1),
            ..Default::default()
        };
        assert_eq!(storage.iter_items(&none).unwrap().count(), 0);
    }

    #[test]
    fn iter_items_excludes_deleted_unless_include_deleted_is_set() {
        let (mut storage, source, run_id) = seeded_storage();
        let mut deleted = prepared("e1", "h1");
        deleted.deleted = true;
        storage
            .upsert_items(source.id, run_id, &[deleted], false, 0)
            .unwrap();

        let live_only = storage.iter_items(&ExportQuery::default()).unwrap().count();
        assert_eq!(live_only, 1);

        let with_deleted = ExportQuery {
            include_deleted: true,
            ..Default::default()
        };
        assert_eq!(storage.iter_items(&with_deleted).unwrap().count(), 2);
    }

    #[test]
    fn iter_revisions_lists_every_revision_across_a_batch() {
        let (mut storage, source, run_id) = seeded_storage();
        storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h1-updated")], false, 0)
            .unwrap();
        let revisions: Vec<ItemRow> = storage
            .iter_revisions(&ExportQuery::default())
            .unwrap()
            .collect();
        // e1 has 2 revisions (created, updated), e2 has 1 (created).
        assert_eq!(revisions.len(), 3);
    }

    #[test]
    fn iter_media_blobs_only_yields_rows_with_archived_bytes() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: Some(b"bytes".to_vec()),
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], true, 0)
            .unwrap();
        let blobs: Vec<ItemRow> = storage
            .iter_media_blobs(&ExportQuery::default())
            .unwrap()
            .collect();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0]["external_id"], Value::from("e1"));
    }

    #[test]
    fn item_counts_reports_total_live_and_deleted() {
        let (mut storage, source, run_id) = seeded_storage();
        let mut deleted = prepared("e1", "h1");
        deleted.deleted = true;
        storage
            .upsert_items(source.id, run_id, &[deleted], false, 0)
            .unwrap();
        assert_eq!(storage.item_counts(source.id).unwrap(), (2, 1, 1));
    }

    #[test]
    fn browse_items_paginates_and_reports_total() {
        let (storage, _, _) = seeded_storage();
        let (rows, total) = storage
            .browse_items(&ExportQuery::default(), None, 1, 0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(total, 2);
    }

    #[test]
    fn browse_items_text_search_matches_title_or_body() {
        let (storage, _, _) = seeded_storage();
        let (rows, total) = storage
            .browse_items(&ExportQuery::default(), Some("cats"), 10, 0)
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0]["external_id"], Value::from("e1"));

        let (_, none_total) = storage
            .browse_items(&ExportQuery::default(), Some("nonexistent"), 10, 0)
            .unwrap();
        assert_eq!(none_total, 0);
    }

    #[test]
    fn fts5_is_enabled_on_a_fresh_in_memory_database() {
        let storage = open();
        assert!(storage.fts_enabled);
    }

    #[test]
    fn fts_match_query_quotes_tokens_and_prefix_matches_the_last_one() {
        assert_eq!(fts_match_query("hello world"), "\"hello\" \"world\"*");
        assert_eq!(fts_match_query("hell"), "\"hell\"*");
        assert_eq!(fts_match_query(""), "\"\"");
        assert_eq!(fts_match_query("say \"hi\""), "\"say\" \"\"\"hi\"\"\"*");
    }

    #[test]
    fn browse_items_fts_search_is_case_insensitive_and_prefix_matches() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        item.title = Some("Hello World".to_string());
        item.body = None;
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();

        let (_, total) = storage
            .browse_items(&ExportQuery::default(), Some("hel"), 10, 0)
            .unwrap();
        assert_eq!(total, 1);
        let (_, total_ci) = storage
            .browse_items(&ExportQuery::default(), Some("WORLD"), 10, 0)
            .unwrap();
        assert_eq!(total_ci, 1);
    }

    #[test]
    fn browse_items_fts_search_requires_every_token() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        item.title = Some("Hello World".to_string());
        item.body = None;
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();

        let (_, total) = storage
            .browse_items(&ExportQuery::default(), Some("hello goodbye"), 10, 0)
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn browse_items_fts_index_stays_in_sync_after_a_title_update() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        item.title = Some("Alpha".to_string());
        item.body = None;
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();
        let (_, before) = storage
            .browse_items(&ExportQuery::default(), Some("alpha"), 10, 0)
            .unwrap();
        assert_eq!(before, 1);

        let mut updated = prepared("e1", "h2");
        updated.title = Some("Beta".to_string());
        updated.body = None;
        storage
            .upsert_items(source.id, run_id, &[updated], false, 0)
            .unwrap();

        let (_, old_term) = storage
            .browse_items(&ExportQuery::default(), Some("alpha"), 10, 0)
            .unwrap();
        assert_eq!(old_term, 0);
        let (_, new_term) = storage
            .browse_items(&ExportQuery::default(), Some("beta"), 10, 0)
            .unwrap();
        assert_eq!(new_term, 1);
    }

    fn browse_thumbnail_for_video_link(video_link: &str) -> Option<String> {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        item.raw_json = serde_json::json!({"videoLink": video_link}).to_string();
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();
        let (rows, _) = storage
            .browse_items(&ExportQuery::default(), None, 10, 0)
            .unwrap();
        rows[0]["thumbnail"].as_str().map(str::to_string)
    }

    #[test]
    fn browse_items_thumbnail_falls_back_to_a_youtube_video_link() {
        let link = "https://youtu.be/dQw4w9WgXcQ";
        assert_eq!(
            browse_thumbnail_for_video_link(link),
            Some(link.to_string())
        );
    }

    #[test]
    fn browse_items_thumbnail_falls_back_to_a_loom_video_link() {
        let link = "https://www.loom.com/share/abc123";
        assert_eq!(
            browse_thumbnail_for_video_link(link),
            Some(link.to_string())
        );
    }

    #[test]
    fn browse_items_thumbnail_falls_back_to_a_vimeo_video_link() {
        let link = "https://vimeo.com/123456789";
        assert_eq!(
            browse_thumbnail_for_video_link(link),
            Some(link.to_string())
        );
    }

    #[test]
    fn browse_items_thumbnail_is_none_without_image_media_or_a_recognized_video_link() {
        assert_eq!(
            browse_thumbnail_for_video_link("https://example.com/not-a-video"),
            None
        );
    }

    #[test]
    fn browse_items_thumbnail_prefers_image_media_over_a_video_link() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        item.raw_json = serde_json::json!({"videoLink": "https://youtu.be/x"}).to_string();
        let media = MediaRef {
            url: "https://example.com/cover.png".to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();
        let (rows, _) = storage
            .browse_items(&ExportQuery::default(), None, 10, 0)
            .unwrap();
        assert_eq!(
            rows[0]["thumbnail"],
            Value::from("https://example.com/cover.png")
        );
    }

    #[test]
    fn get_item_includes_media_and_full_raw_payload() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();

        let (rows, _) = storage
            .browse_items(&ExportQuery::default(), None, 10, 0)
            .unwrap();
        let id = rows[0]["id"].as_i64().unwrap();

        let fetched = storage.get_item(id).unwrap().unwrap();
        assert_eq!(fetched["external_id"], Value::from("e1"));
        assert_eq!(fetched["media"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_item_returns_none_for_an_unknown_id() {
        let storage = open();
        assert!(storage.get_item(999).unwrap().is_none());
    }

    #[test]
    fn get_media_blob_round_trips_archived_bytes() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: Some("x.png".to_string()),
            mime: Some("image/png".to_string()),
            data: Some(b"hello".to_vec()),
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], true, 0)
            .unwrap();

        let media_id: i64 = storage
            .conn
            .query_row("SELECT id FROM media", [], |r| r.get(0))
            .unwrap();
        let blob = storage.get_media_blob(media_id).unwrap().unwrap();
        assert_eq!(blob["filename"], Value::from("x.png"));
        assert_eq!(
            blob["data"],
            serde_json::to_value(b"hello".to_vec()).unwrap()
        );
    }

    #[test]
    fn get_media_blob_returns_none_when_bytes_were_never_archived() {
        let mut storage = open();
        let source = storage.upsert_source("a", "t", "p", "{}", 1).unwrap();
        let run_id = storage
            .begin_run(source.id, "p", "incremental", None)
            .unwrap();
        let mut item = prepared("e1", "h1");
        let media = MediaRef {
            url: "https://example.com/x.png".to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        };
        item.media = vec![serde_json::to_value(&media).unwrap()];
        storage
            .upsert_items(source.id, run_id, &[item], false, 0)
            .unwrap();
        let media_id: i64 = storage
            .conn
            .query_row("SELECT id FROM media", [], |r| r.get(0))
            .unwrap();
        assert!(storage.get_media_blob(media_id).unwrap().is_none());
    }

    #[test]
    fn metrics_aggregates_by_source_and_kind() {
        let (storage, _, _) = seeded_storage();
        let metrics = storage.metrics().unwrap();
        let by_source_kind = metrics["by_source_kind"].as_array().unwrap();
        assert_eq!(by_source_kind.len(), 1);
        assert_eq!(by_source_kind[0]["total"], Value::from(2));
        assert_eq!(by_source_kind[0]["live"], Value::from(2));
        assert_eq!(metrics["revision_count"], Value::from(2));
    }

    #[test]
    fn maintain_reports_wal_checkpoint_and_optimize_without_vacuum() {
        let (mut storage, _, _) = seeded_storage();
        let report = storage.maintain(false).unwrap();
        assert_eq!(report["optimized"], Value::from(true));
        assert_eq!(report["vacuumed"], Value::from(false));
        assert!(report.contains_key("size_before"));
    }

    #[test]
    fn maintain_with_vacuum_reports_vacuumed_true() {
        let (mut storage, _, _) = seeded_storage();
        let report = storage.maintain(true).unwrap();
        assert_eq!(report["vacuumed"], Value::from(true));
    }

    #[test]
    fn prune_revisions_keeps_only_the_newest_n_and_is_a_noop_at_zero() {
        let (mut storage, source, run_id) = seeded_storage();
        storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h1-v2")], false, 0)
            .unwrap();
        storage
            .upsert_items(source.id, run_id, &[prepared("e1", "h1-v3")], false, 0)
            .unwrap();
        // e1 now has 3 revisions, e2 has 1 — 4 total.
        assert_eq!(storage.prune_revisions(source.id, 0).unwrap(), 0);
        let pruned = storage.prune_revisions(source.id, 1).unwrap();
        assert_eq!(pruned, 2); // e1's two oldest revisions removed
        let remaining: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM item_revisions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 2); // e1's newest + e2's only revision
    }

    #[test]
    fn vacuum_into_writes_a_snapshot_and_refuses_an_existing_target() {
        let (storage, _, _) = seeded_storage();
        let dir = std::env::temp_dir().join(format!(
            "rusty_dbs_sqlite_storage_vacuum_test_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("snapshot.sqlite3");

        let size = storage.vacuum_into(&dest).unwrap();
        assert!(size > 0);
        assert!(dest.exists());

        let err = storage.vacuum_into(&dest).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
