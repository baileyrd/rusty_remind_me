//! Concrete `Storage` implementation over SQLite.
//!
//! Mirrors `SqliteStorage` in `src/dbs/storage/sqlite.py` in
//! baileyrd/Daily-Backup-System (pinned `@6cc6491`). Issue #36 is large
//! (~1100 lines in the reference) and its own acceptance checklist calls
//! for splitting the work across multiple PRs by trait section; this PR
//! covers **schema lifecycle, sources, runs, cursor/state, and locking**.
//! The remaining sections — items/upsert (the largest and most
//! correctness-sensitive part), and export/browse/stats/maintenance — are
//! stubbed here (`Err(DbsError::Storage(...))`) and land in follow-up
//! PRs against the same issue.
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

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::errors::DbsError;
use crate::models::Cursor;
use crate::storage::sqlite::open_connection;
use crate::storage::{BatchResult, ExportQuery, ItemRow, PreparedItem, SourceRecord, Storage};
use crate::timeutil::{iso_z, parse_iso};

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
    // Implemented in a follow-up PR against issue #36.

    fn upsert_items(
        &mut self,
        _source_id: i64,
        _run_id: i64,
        _items: &[PreparedItem],
        _store_media: bool,
        _max_media_bytes: u64,
    ) -> Result<BatchResult, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::upsert_items is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn soft_delete_missing(
        &mut self,
        _source_id: i64,
        _live_ids: &HashSet<String>,
        _run_id: i64,
        _tag: Option<&str>,
    ) -> Result<u64, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::soft_delete_missing is not yet implemented (see issue #36)".to_string(),
        ))
    }

    fn live_external_ids(
        &self,
        _source_id: i64,
        _tag: Option<&str>,
    ) -> Result<HashSet<String>, DbsError> {
        Err(DbsError::Storage(
            "SqliteStorage::live_external_ids is not yet implemented (see issue #36)".to_string(),
        ))
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
    fn stubbed_item_and_export_methods_return_storage_errors() {
        let storage = open();
        assert!(storage
            .live_external_ids(1, None)
            .unwrap_err()
            .to_string()
            .contains("not yet implemented"));
        assert!(storage.item_counts(1).is_err());
        assert!(storage.get_item(1).unwrap_err().to_string().contains("#36"));
    }
}
