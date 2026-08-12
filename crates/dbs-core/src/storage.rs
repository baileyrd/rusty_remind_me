//! Storage abstraction.
//!
//! Mirrors `src/dbs/storage/base.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Only the engine and service talk to storage;
//! connectors never do. Keeping this a trait means a future backend can
//! swap SQLite for something else without touching the core.
//!
//! Scope notes (this issue defines the trait and its supporting types
//! only — a concrete SQLite implementation is #12, which is also where
//! `rusqlite` gets added as a dependency; this module needs none):
//!
//! * `iter_items`/`browse_items`/`iter_revisions` take an [`ExportQuery`]
//!   defined *here* as a minimal placeholder, not the reference's real
//!   `export/base.py::ExportQuery`, which is its own gap-analysis row.
//!   Superseded when that issue lands.
//! * The reference's `transaction()` context-manager method isn't ported.
//!   Rust has no direct equivalent for a trait-object-safe RAII guard
//!   scoped to an unknown concrete connection type without more design
//!   work than this issue's scope covers — atomicity for a batch write
//!   (`upsert_items`, etc.) is instead each such method's own
//!   responsibility in the concrete (#12) implementation. Revisit if a
//!   real cross-method transaction need surfaces.
//! * Fallible operations return `Result<_, DbsError>` (via the new
//!   `DbsError::Storage` variant) — Python lets backend exceptions
//!   propagate unchecked, but Rust doesn't have unchecked exceptions.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::errors::DbsError;
use crate::models::Cursor;

/// An item normalized by the engine and ready to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedItem {
    pub external_id: String,
    pub item_kind: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub body: Option<String>,
    pub tags: Vec<String>,
    /// ISO-8601 `Z`, via `crate::timeutil::iso_z`.
    pub item_created_at: Option<String>,
    pub item_updated_at: Option<String>,
    pub content_hash: String,
    pub raw_json: String,
    pub deleted: bool,
    pub media: Vec<Value>,
}

/// Classification counts for one [`Storage::upsert_items`] call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchResult {
    pub created: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub undeleted: u64,
    pub revisions: u64,
    pub max_updated_at: Option<String>,
}

impl BatchResult {
    /// Accumulates `other` into `self`, keeping the lexicographically
    /// (== chronologically, per `timeutil`'s canonical `Z`-suffixed form)
    /// greater `max_updated_at`.
    pub fn merge(&mut self, other: &BatchResult) {
        self.created += other.created;
        self.updated += other.updated;
        self.unchanged += other.unchanged;
        self.deleted += other.deleted;
        self.undeleted += other.undeleted;
        self.revisions += other.revisions;
        if let Some(other_max) = &other.max_updated_at {
            match &self.max_updated_at {
                Some(current_max) if current_max >= other_max => {}
                _ => self.max_updated_at = Some(other_max.clone()),
            }
        }
    }
}

/// An export/browse row. Kept loose on purpose, matching the reference's
/// plain-dict rows.
pub type ItemRow = HashMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct SourceRecord {
    pub id: i64,
    pub name: String,
    pub type_: String,
    pub plugin_id: String,
    pub config_json: String,
    pub schema_version: u32,
    pub enabled: bool,
    pub created_at: String,
}

/// Minimal placeholder standing in for the reference's
/// `export/base.py::ExportQuery` — see the module doc-comment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportQuery {
    pub source_id: Option<i64>,
    pub item_kind: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub include_deleted: bool,
}

/// Persistence contract for the engine/service.
pub trait Storage {
    /// Applies any pending schema migrations. Idempotent.
    fn migrate(&mut self) -> Result<(), DbsError>;

    fn close(&mut self);

    /// A new, independent connection to the same underlying database, for
    /// a worker thread (`backup --all --parallel N`). The caller owns the
    /// returned storage and must close it. `None` means this backend
    /// can't provide one and the caller must fall back to sequential
    /// execution.
    fn spawn(&self) -> Option<Box<dyn Storage>> {
        None
    }

    // -- sources ----------------------------------------------------------

    fn upsert_source(
        &mut self,
        name: &str,
        type_: &str,
        plugin_id: &str,
        config_json: &str,
        schema_version: u32,
    ) -> Result<SourceRecord, DbsError>;

    fn get_source(&self, name: &str) -> Result<Option<SourceRecord>, DbsError>;

    fn list_sources(&self) -> Result<Vec<SourceRecord>, DbsError>;

    fn delete_source(&mut self, name: &str) -> Result<bool, DbsError>;

    // -- runs ---------------------------------------------------------------

    fn begin_run(
        &mut self,
        source_id: i64,
        plugin_id: &str,
        mode: &str,
        cursor_before: Option<&str>,
    ) -> Result<i64, DbsError>;

    #[allow(clippy::too_many_arguments)]
    fn finish_run(
        &mut self,
        run_id: i64,
        status: &str,
        stats: &BatchResult,
        items_seen: u64,
        cursor_after: Option<&str>,
        error: Option<&str>,
        warnings: &[String],
    ) -> Result<(), DbsError>;

    /// Marks stale `running` runs as `interrupted` (crash recovery).
    /// Returns the affected run ids.
    fn reap_interrupted_runs(&mut self) -> Result<Vec<i64>, DbsError>;

    fn recent_runs(&self, source_id: Option<i64>, limit: u32) -> Result<Vec<ItemRow>, DbsError>;

    // -- items / batch commit ------------------------------------------------

    /// Idempotently persists a batch, classifying each item. When
    /// `store_media` is set, local-file media references are archived
    /// inline (up to `max_media_bytes` per file; 0 = no limit).
    fn upsert_items(
        &mut self,
        source_id: i64,
        run_id: i64,
        items: &[PreparedItem],
        store_media: bool,
        max_media_bytes: u64,
    ) -> Result<BatchResult, DbsError>;

    /// Soft-deletes non-deleted items absent from `live_ids`. Returns the
    /// count. With `tag`, only items carrying that tag are candidates —
    /// the storage half of a tag-scoped `ReconcileMarker`.
    fn soft_delete_missing(
        &mut self,
        source_id: i64,
        live_ids: &std::collections::HashSet<String>,
        run_id: i64,
        tag: Option<&str>,
    ) -> Result<u64, DbsError>;

    /// Currently-live (non-deleted) external ids for a source. With
    /// `tag`, only ids of items carrying that tag are returned.
    fn live_external_ids(
        &self,
        source_id: i64,
        tag: Option<&str>,
    ) -> Result<std::collections::HashSet<String>, DbsError>;

    // -- cursor / state -------------------------------------------------------

    fn save_cursor(
        &mut self,
        source_id: i64,
        cursor: Option<&Cursor>,
        watermark: Option<&str>,
        run_id: i64,
    ) -> Result<(), DbsError>;

    fn load_cursor(
        &self,
        source_id: i64,
    ) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError>;

    fn get_run_count(&self, source_id: i64) -> Result<u64, DbsError>;

    fn increment_run_count(&mut self, source_id: i64) -> Result<(), DbsError>;

    // -- locking --------------------------------------------------------------

    fn acquire_lock(&mut self, source_id: i64, run_id: i64) -> Result<bool, DbsError>;

    fn release_lock(&mut self, source_id: i64) -> Result<(), DbsError>;

    // -- export / stats ---------------------------------------------------------

    fn iter_items<'a>(
        &'a self,
        query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError>;

    fn iter_revisions<'a>(
        &'a self,
        query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError>;

    /// Archived media blobs (only items with stored bytes). Default is
    /// empty so backends that don't archive media bytes need not
    /// implement it.
    fn iter_media_blobs<'a>(
        &'a self,
        _query: &ExportQuery,
    ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
        Ok(Box::new(std::iter::empty()))
    }

    /// `(total, live, deleted)` item counts for a source.
    fn item_counts(&self, source_id: i64) -> Result<(u64, u64, u64), DbsError>;

    /// Paginated item listing for the web UI. Returns `(rows,
    /// total_matching)`. `text` matches against title/body, in addition
    /// to `query`'s source/type/date/deleted filters.
    fn browse_items(
        &self,
        query: &ExportQuery,
        text: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<ItemRow>, u64), DbsError>;

    /// Full detail for one item (raw payload + its media list), by
    /// internal id.
    fn get_item(&self, item_id: i64) -> Result<Option<ItemRow>, DbsError>;

    /// One archived media blob (bytes + mime/filename) by id. `None` if
    /// the media row doesn't exist or its bytes were never archived.
    fn get_media_blob(&self, media_id: i64) -> Result<Option<ItemRow>, DbsError>;

    /// Aggregate item/media/revision counts for the web UI's metrics
    /// strip.
    fn metrics(&self) -> Result<ItemRow, DbsError>;

    fn integrity_check(&self) -> Result<String, DbsError>;

    /// Housekeeping pass (e.g. WAL checkpoint, planner stats, VACUUM).
    /// Backend-specific; the SQLite implementation (#12) overrides this.
    fn maintain(&mut self, _vacuum: bool) -> Result<ItemRow, DbsError> {
        Ok(ItemRow::new())
    }

    /// Deletes all but the newest `keep` revisions of each of the
    /// source's items (0 = keep everything). Returns rows deleted. Items
    /// themselves are never touched.
    fn prune_revisions(&mut self, _source_id: i64, _keep: u32) -> Result<u64, DbsError> {
        Ok(0)
    }

    /// Writes a consistent single-file snapshot to `dest` (must not
    /// exist) and returns its size in bytes.
    fn vacuum_into(&self, _dest: &Path) -> Result<u64, DbsError> {
        Err(DbsError::Storage(
            "this backend does not support snapshots".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_result_merge_sums_counts() {
        let mut a = BatchResult {
            created: 1,
            updated: 2,
            ..Default::default()
        };
        let b = BatchResult {
            created: 3,
            unchanged: 4,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.created, 4);
        assert_eq!(a.updated, 2);
        assert_eq!(a.unchanged, 4);
    }

    #[test]
    fn batch_result_merge_keeps_the_later_max_updated_at() {
        let mut a = BatchResult {
            max_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let earlier = BatchResult {
            max_updated_at: Some("2025-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let later = BatchResult {
            max_updated_at: Some("2027-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        a.merge(&earlier);
        assert_eq!(a.max_updated_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        a.merge(&later);
        assert_eq!(a.max_updated_at.as_deref(), Some("2027-01-01T00:00:00Z"));
    }

    #[test]
    fn batch_result_merge_handles_none_max_updated_at() {
        let mut a = BatchResult::default();
        let b = BatchResult {
            max_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.max_updated_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    /// An in-memory `Storage` implementation, standing in for a real
    /// backend — exercises the trait's shape and confirms it's
    /// object-safe (`Box<dyn Storage>`), same pattern as `Connector`'s
    /// `FakeConnector` in issue #4.
    #[derive(Default)]
    struct InMemoryStorage {
        sources: HashMap<String, SourceRecord>,
        next_id: i64,
    }

    impl Storage for InMemoryStorage {
        fn migrate(&mut self) -> Result<(), DbsError> {
            Ok(())
        }

        fn close(&mut self) {}

        fn upsert_source(
            &mut self,
            name: &str,
            type_: &str,
            plugin_id: &str,
            config_json: &str,
            schema_version: u32,
        ) -> Result<SourceRecord, DbsError> {
            self.next_id += 1;
            let record = SourceRecord {
                id: self.next_id,
                name: name.to_string(),
                type_: type_.to_string(),
                plugin_id: plugin_id.to_string(),
                config_json: config_json.to_string(),
                schema_version,
                enabled: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            };
            self.sources.insert(name.to_string(), record.clone());
            Ok(record)
        }

        fn get_source(&self, name: &str) -> Result<Option<SourceRecord>, DbsError> {
            Ok(self.sources.get(name).cloned())
        }

        fn list_sources(&self) -> Result<Vec<SourceRecord>, DbsError> {
            Ok(self.sources.values().cloned().collect())
        }

        fn delete_source(&mut self, name: &str) -> Result<bool, DbsError> {
            Ok(self.sources.remove(name).is_some())
        }

        fn begin_run(
            &mut self,
            _source_id: i64,
            _plugin_id: &str,
            _mode: &str,
            _cursor_before: Option<&str>,
        ) -> Result<i64, DbsError> {
            Ok(1)
        }

        fn finish_run(
            &mut self,
            _run_id: i64,
            _status: &str,
            _stats: &BatchResult,
            _items_seen: u64,
            _cursor_after: Option<&str>,
            _error: Option<&str>,
            _warnings: &[String],
        ) -> Result<(), DbsError> {
            Ok(())
        }

        fn reap_interrupted_runs(&mut self) -> Result<Vec<i64>, DbsError> {
            Ok(Vec::new())
        }

        fn recent_runs(
            &self,
            _source_id: Option<i64>,
            _limit: u32,
        ) -> Result<Vec<ItemRow>, DbsError> {
            Ok(Vec::new())
        }

        fn upsert_items(
            &mut self,
            _source_id: i64,
            _run_id: i64,
            items: &[PreparedItem],
            _store_media: bool,
            _max_media_bytes: u64,
        ) -> Result<BatchResult, DbsError> {
            Ok(BatchResult {
                created: items.len() as u64,
                ..Default::default()
            })
        }

        fn soft_delete_missing(
            &mut self,
            _source_id: i64,
            _live_ids: &std::collections::HashSet<String>,
            _run_id: i64,
            _tag: Option<&str>,
        ) -> Result<u64, DbsError> {
            Ok(0)
        }

        fn live_external_ids(
            &self,
            _source_id: i64,
            _tag: Option<&str>,
        ) -> Result<std::collections::HashSet<String>, DbsError> {
            Ok(std::collections::HashSet::new())
        }

        fn save_cursor(
            &mut self,
            _source_id: i64,
            _cursor: Option<&Cursor>,
            _watermark: Option<&str>,
            _run_id: i64,
        ) -> Result<(), DbsError> {
            Ok(())
        }

        fn load_cursor(
            &self,
            _source_id: i64,
        ) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError> {
            Ok((None, None))
        }

        fn get_run_count(&self, _source_id: i64) -> Result<u64, DbsError> {
            Ok(0)
        }

        fn increment_run_count(&mut self, _source_id: i64) -> Result<(), DbsError> {
            Ok(())
        }

        fn acquire_lock(&mut self, _source_id: i64, _run_id: i64) -> Result<bool, DbsError> {
            Ok(true)
        }

        fn release_lock(&mut self, _source_id: i64) -> Result<(), DbsError> {
            Ok(())
        }

        fn iter_items<'a>(
            &'a self,
            _query: &ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            Ok(Box::new(std::iter::empty()))
        }

        fn iter_revisions<'a>(
            &'a self,
            _query: &ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            Ok(Box::new(std::iter::empty()))
        }

        fn item_counts(&self, _source_id: i64) -> Result<(u64, u64, u64), DbsError> {
            Ok((0, 0, 0))
        }

        fn browse_items(
            &self,
            _query: &ExportQuery,
            _text: Option<&str>,
            _limit: u32,
            _offset: u32,
        ) -> Result<(Vec<ItemRow>, u64), DbsError> {
            Ok((Vec::new(), 0))
        }

        fn get_item(&self, _item_id: i64) -> Result<Option<ItemRow>, DbsError> {
            Ok(None)
        }

        fn get_media_blob(&self, _media_id: i64) -> Result<Option<ItemRow>, DbsError> {
            Ok(None)
        }

        fn metrics(&self) -> Result<ItemRow, DbsError> {
            Ok(ItemRow::new())
        }

        fn integrity_check(&self) -> Result<String, DbsError> {
            Ok("ok".to_string())
        }
    }

    #[test]
    fn storage_is_object_safe_and_round_trips_a_source() {
        let mut storage: Box<dyn Storage> = Box::new(InMemoryStorage::default());
        let record = storage
            .upsert_source("raindrop", "raindrop", "rusty_dbs:raindrop", "{}", 1)
            .unwrap();
        assert_eq!(record.name, "raindrop");
        assert_eq!(
            storage.get_source("raindrop").unwrap().unwrap().id,
            record.id
        );
        assert!(storage.get_source("missing").unwrap().is_none());
    }

    #[test]
    fn default_maintain_prune_vacuum_into_match_reference_defaults() {
        let mut storage = InMemoryStorage::default();
        assert_eq!(storage.maintain(false).unwrap(), ItemRow::new());
        assert_eq!(storage.prune_revisions(1, 5).unwrap(), 0);
        assert!(storage.vacuum_into(Path::new("/tmp/x")).is_err());
    }

    #[test]
    fn spawn_defaults_to_none() {
        let storage = InMemoryStorage::default();
        assert!(storage.spawn().is_none());
    }
}
