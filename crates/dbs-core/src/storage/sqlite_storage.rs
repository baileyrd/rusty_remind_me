//! Concrete `Storage` implementation over SQLite.
//!
//! Mirrors `SqliteStorage` in `src/dbs/storage/sqlite.py` in
//! baileyrd/Daily-Backup-System (pinned `@6cc6491`). Issue #36 is large
//! (~1100 lines in the reference) and its own acceptance checklist calls
//! for splitting the work across multiple PRs by trait section. Landed so
//! far: schema lifecycle, sources, runs, cursor/state, locking (first
//! PR), and **items/batch commit — `upsert_items`/`soft_delete_missing`/
//! `live_external_ids`, including media archiving** (this PR). Still
//! stubbed (`Err(DbsError::Storage(...))`), pending a follow-up PR:
//! export/browse/stats/maintenance.
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

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
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
}

impl SqliteStorage {
    /// Opens (creating/migrating as needed) a database at `path`, or a
    /// private in-memory database for `":memory:"`/`""`/`file::memory:`.
    pub fn open(path: &str) -> Result<Self, DbsError> {
        let conn = open_connection(path)?;
        Ok(Self {
            path: path.to_string(),
            conn,
        })
    }

    fn now(&self) -> String {
        iso_z(Utc::now())
    }
}

impl Storage for SqliteStorage {
    fn migrate(&mut self) -> Result<(), DbsError> {
        crate::storage::migrations::migrate(&mut self.conn)?;
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
    // Implemented in a follow-up PR against issue #36.

    fn iter_items<'a>(
        &'a self,
        _query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::iter_items is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn iter_revisions<'a>(
        &'a self,
        _query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::iter_revisions is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn item_counts(&self, _source_id: i64) -> Result<(u64, u64, u64), DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::item_counts is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn browse_items(
        &self,
        _query: &ExportQuery,
        _text: Option<&str>,
        _limit: u32,
        _offset: u32,
    ) -> Result<(Vec<ItemRow>, u64), DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::browse_items is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn get_item(&self, _item_id: i64) -> Result<Option<ItemRow>, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::get_item is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn get_media_blob(&self, _media_id: i64) -> Result<Option<ItemRow>, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::get_media_blob is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn metrics(&self) -> Result<ItemRow, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::metrics is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn integrity_check(&self) -> Result<String, DbsError> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .map_err(|e| storage_err("failed to run integrity_check", e))
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

    #[test]
    fn stubbed_export_methods_return_storage_errors() {
        let storage = open();
        assert!(storage.item_counts(1).is_err());
        assert!(storage.get_item(1).unwrap_err().to_string().contains("#36"));
        assert!(storage.metrics().is_err());
        assert!(storage.get_media_blob(1).is_err());
        assert!(storage
            .browse_items(&ExportQuery::default(), None, 10, 0)
            .is_err());
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
}
