//! The backup engine — drives a connector's fetch stream into storage,
//! enforcing the correctness invariants the reference documents in
//! `docs/architecture.md`'s "Anatomy of a backup run".
//!
//! Mirrors `src/dbs/core/engine.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). This module owns *orchestration* — the order
//! operations happen in — while [`crate::storage::Storage`] owns
//! persistence; that split matches the reference's own
//! `dbs.core.engine`/`dbs.storage.base` module boundary. `Storage`'s
//! trait surface (#11) is unchanged by this issue.
//!
//! **This issue's scope: invariant #1 only** — "the cursor never gets
//! ahead of data." [`commit_checkpoint`] persists buffered items *before*
//! saving the new cursor, so a crash between the two calls leaves the
//! cursor lagging committed data (safe — the next run re-fetches the
//! overlap and idempotent upsert dedups it) and never advances the cursor
//! past data that was never durably written (unsafe — permanent data
//! loss). The reference wraps both calls in one DB transaction for
//! stronger guarantees (atomicity *within* the upsert batch itself, not
//! just the ordering); this round's `Storage` trait deliberately has no
//! `transaction()` combinator (#11's own scope note), so real
//! per-backend atomicity is up to the concrete `SqliteStorage` (#36) to
//! add if/when it proves necessary — the ordering invariant this issue
//! covers holds either way.
//!
//! Invariants #2 (idempotent upsert classification) through #5 (crash
//! recovery reaper) are separate issues (#17/#19/#20/#21) building on
//! top of this.

use crate::errors::DbsError;
use crate::models::Checkpoint;
use crate::storage::{BatchResult, PreparedItem, Storage};

/// Persists `buffered_items` and then `checkpoint`'s cursor, in that
/// order — never the reverse. Returns the batch's classification counts.
///
/// `watermark` is derived from the committed batch's `max_updated_at`
/// (the engine's watermark = max(updated_at) committed so far, per the
/// reference).
pub fn commit_checkpoint(
    storage: &mut dyn Storage,
    source_id: i64,
    run_id: i64,
    buffered_items: &[PreparedItem],
    checkpoint: &Checkpoint,
    store_media: bool,
    max_media_bytes: u64,
) -> Result<BatchResult, DbsError> {
    let result = storage.upsert_items(
        source_id,
        run_id,
        buffered_items,
        store_media,
        max_media_bytes,
    )?;
    storage.save_cursor(
        source_id,
        Some(&checkpoint.cursor),
        result.max_updated_at.as_deref(),
        run_id,
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Cursor;
    use crate::storage::{ExportQuery, ItemRow, SourceRecord};
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::collections::HashSet;

    /// A capable-enough in-memory `Storage` test double: actually tracks
    /// upserted-item counts and the saved cursor, so tests can observe
    /// the checkpoint-commit invariant rather than just compiling
    /// against the trait shape (contrast with #11's minimal
    /// `InMemoryStorage`, which doesn't persist state at all).
    #[derive(Default)]
    struct TrackingStorage {
        items_committed: usize,
        cursor: Option<Cursor>,
        /// When `Some(n)`, the *n*th call to `save_cursor` fails instead
        /// of succeeding — simulates a crash between the upsert commit
        /// and the cursor save.
        fail_save_cursor_on_call: Option<usize>,
        save_cursor_calls: usize,
        last_watermark: Option<String>,
    }

    impl Storage for TrackingStorage {
        fn migrate(&mut self) -> Result<(), DbsError> {
            Ok(())
        }
        fn close(&mut self) {}

        fn upsert_source(
            &mut self,
            _name: &str,
            _type_: &str,
            _plugin_id: &str,
            _config_json: &str,
            _schema_version: u32,
        ) -> Result<SourceRecord, DbsError> {
            unimplemented!("not exercised by this issue's tests")
        }
        fn get_source(&self, _name: &str) -> Result<Option<SourceRecord>, DbsError> {
            unimplemented!()
        }
        fn list_sources(&self) -> Result<Vec<SourceRecord>, DbsError> {
            unimplemented!()
        }
        fn delete_source(&mut self, _name: &str) -> Result<bool, DbsError> {
            unimplemented!()
        }
        fn begin_run(
            &mut self,
            _source_id: i64,
            _plugin_id: &str,
            _mode: &str,
            _cursor_before: Option<&str>,
        ) -> Result<i64, DbsError> {
            unimplemented!()
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
            unimplemented!()
        }
        fn reap_interrupted_runs(&mut self) -> Result<Vec<i64>, DbsError> {
            unimplemented!()
        }
        fn recent_runs(
            &self,
            _source_id: Option<i64>,
            _limit: u32,
        ) -> Result<Vec<ItemRow>, DbsError> {
            unimplemented!()
        }

        fn upsert_items(
            &mut self,
            _source_id: i64,
            _run_id: i64,
            items: &[PreparedItem],
            _store_media: bool,
            _max_media_bytes: u64,
        ) -> Result<BatchResult, DbsError> {
            self.items_committed += items.len();
            Ok(BatchResult {
                created: items.len() as u64,
                max_updated_at: items.iter().filter_map(|i| i.item_updated_at.clone()).max(),
                ..Default::default()
            })
        }

        fn soft_delete_missing(
            &mut self,
            _source_id: i64,
            _live_ids: &HashSet<String>,
            _run_id: i64,
            _tag: Option<&str>,
        ) -> Result<u64, DbsError> {
            unimplemented!()
        }
        fn live_external_ids(
            &self,
            _source_id: i64,
            _tag: Option<&str>,
        ) -> Result<HashSet<String>, DbsError> {
            unimplemented!()
        }

        fn save_cursor(
            &mut self,
            _source_id: i64,
            cursor: Option<&Cursor>,
            watermark: Option<&str>,
            _run_id: i64,
        ) -> Result<(), DbsError> {
            self.save_cursor_calls += 1;
            if self.fail_save_cursor_on_call == Some(self.save_cursor_calls) {
                return Err(DbsError::Storage(
                    "simulated crash before cursor commit".to_string(),
                ));
            }
            self.cursor = cursor.cloned();
            self.last_watermark = watermark.map(str::to_string);
            Ok(())
        }

        fn load_cursor(
            &self,
            _source_id: i64,
        ) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError> {
            Ok((self.cursor.clone(), None))
        }

        fn get_run_count(&self, _source_id: i64) -> Result<u64, DbsError> {
            unimplemented!()
        }
        fn increment_run_count(&mut self, _source_id: i64) -> Result<(), DbsError> {
            unimplemented!()
        }
        fn acquire_lock(&mut self, _source_id: i64, _run_id: i64) -> Result<bool, DbsError> {
            unimplemented!()
        }
        fn release_lock(&mut self, _source_id: i64) -> Result<(), DbsError> {
            unimplemented!()
        }
        fn iter_items<'a>(
            &'a self,
            _query: &ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            unimplemented!()
        }
        fn iter_revisions<'a>(
            &'a self,
            _query: &ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            unimplemented!()
        }
        fn item_counts(&self, _source_id: i64) -> Result<(u64, u64, u64), DbsError> {
            unimplemented!()
        }
        fn browse_items(
            &self,
            _query: &ExportQuery,
            _text: Option<&str>,
            _limit: u32,
            _offset: u32,
        ) -> Result<(Vec<ItemRow>, u64), DbsError> {
            unimplemented!()
        }
        fn get_item(&self, _item_id: i64) -> Result<Option<ItemRow>, DbsError> {
            unimplemented!()
        }
        fn get_media_blob(&self, _media_id: i64) -> Result<Option<ItemRow>, DbsError> {
            unimplemented!()
        }
        fn metrics(&self) -> Result<ItemRow, DbsError> {
            unimplemented!()
        }
        fn integrity_check(&self) -> Result<String, DbsError> {
            unimplemented!()
        }
    }

    fn item(id: &str) -> PreparedItem {
        PreparedItem {
            external_id: id.to_string(),
            item_kind: "post".to_string(),
            title: None,
            url: None,
            body: None,
            tags: Vec::new(),
            item_created_at: None,
            item_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            content_hash: "deadbeef".to_string(),
            raw_json: json!({}).to_string(),
            deleted: false,
            media: Vec::new(),
        }
    }

    fn checkpoint(after: &str) -> Checkpoint {
        Checkpoint {
            cursor: Cursor {
                value: json!({"after": after}),
            },
            note: String::new(),
        }
    }

    #[test]
    fn commit_checkpoint_persists_items_then_cursor() {
        let mut storage = TrackingStorage::default();
        let items = vec![item("1"), item("2")];
        let result =
            commit_checkpoint(&mut storage, 1, 1, &items, &checkpoint("2"), false, 0).unwrap();
        assert_eq!(result.created, 2);
        assert_eq!(storage.items_committed, 2);
        assert_eq!(
            storage.load_cursor(1).unwrap().0,
            Some(Cursor {
                value: json!({"after": "2"})
            })
        );
    }

    #[test]
    fn crash_between_upsert_and_save_cursor_leaves_the_cursor_lagging_not_ahead() {
        let mut storage = TrackingStorage {
            fail_save_cursor_on_call: Some(1),
            ..Default::default()
        };
        let items = vec![item("1")];
        let err =
            commit_checkpoint(&mut storage, 1, 1, &items, &checkpoint("1"), false, 0).unwrap_err();
        assert!(matches!(err, DbsError::Storage(_)));

        // The item was durably committed before the simulated crash...
        assert_eq!(storage.items_committed, 1);
        // ...but the cursor was never advanced to reflect it — the next
        // run resumes from the old (here: absent) cursor and re-fetches
        // the overlap, which idempotent upsert (#17) will safely dedup.
        assert_eq!(storage.load_cursor(1).unwrap().0, None);
    }

    #[test]
    fn a_later_checkpoint_after_a_recovered_crash_succeeds_normally() {
        let mut storage = TrackingStorage {
            fail_save_cursor_on_call: Some(1),
            ..Default::default()
        };
        let items = vec![item("1")];
        assert!(commit_checkpoint(&mut storage, 1, 1, &items, &checkpoint("1"), false, 0).is_err());

        // "Next run" — re-fetches the same item (idempotent, #17's
        // concern) and this time the cursor save succeeds.
        let result =
            commit_checkpoint(&mut storage, 1, 2, &items, &checkpoint("1"), false, 0).unwrap();
        assert_eq!(result.created, 1);
        assert_eq!(
            storage.load_cursor(1).unwrap().0,
            Some(Cursor {
                value: json!({"after": "1"})
            })
        );
    }

    #[test]
    fn watermark_is_derived_from_the_committed_batch_max_updated_at() {
        let mut storage = TrackingStorage::default();
        let items = vec![item("1")];
        commit_checkpoint(&mut storage, 1, 1, &items, &checkpoint("1"), false, 0).unwrap();
        assert_eq!(
            storage.last_watermark.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }
}
