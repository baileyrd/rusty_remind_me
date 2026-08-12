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
//! **Issue #16's scope: invariant #1** — "the cursor never gets ahead of
//! data." [`commit_checkpoint`] persists buffered items *before* saving
//! the new cursor, so a crash between the two calls leaves the cursor
//! lagging committed data (safe — the next run re-fetches the overlap
//! and idempotent upsert dedups it) and never advances the cursor past
//! data that was never durably written (unsafe — permanent data loss).
//! The reference wraps both calls in one DB transaction for stronger
//! guarantees (atomicity *within* the upsert batch itself, not just the
//! ordering); this round's `Storage` trait deliberately has no
//! `transaction()` combinator (#11's own scope note), so real
//! per-backend atomicity is up to the concrete `SqliteStorage` (#36) to
//! add if/when it proves necessary — the ordering invariant this issue
//! covers holds either way.
//!
//! **Issue #17's scope: preparing a `BackupItem` for storage**
//! ([`prepare`]/[`compute_hash`]) — validating `item_kind` against the
//! connector's declared kinds, computing the content hash (a
//! `revision_token` shortcut when the connector supplies one, otherwise
//! a normalized projection with volatile fields stripped), and
//! formatting timestamps. This is genuinely engine-side logic in the
//! reference (`Engine._prepare`/`Engine._compute_hash` in
//! `core/engine.py`) — the created/updated/unchanged/deleted/undeleted
//! *classification* itself (comparing the computed hash against what's
//! already stored) is backend-specific SQL in the reference's
//! `SqliteStorage._update_item`, so it belongs to #36
//! (`SqliteStorage`), not here. #17's title ("idempotent upsert
//! classification") undersold this — the classification decision is
//! storage's job; the engine's job is producing what gets compared.
//!
//! **Issue #20's scope: the deletion-sweep safety decision**
//! ([`sweep_deletions`]) — per reconcile scope, compares the connector's
//! full-enumeration result against what storage still has live, and
//! refuses to sweep (recording a warning instead) when the deletion
//! would be implausibly large — almost certainly a truncated upstream
//! listing rather than genuine mass deletion. Callers gate this on
//! `ctx.mode in ("full", "reconcile") and caps.supports_full_enumeration`
//! before calling it (that precondition lives in the future full
//! `run_source` loop, not here — same "extract the well-defined,
//! testable piece" pattern as #16/#17).
//!
//! Invariant #3 (revision writing) turned out to have no independent
//! engine-side content — same discovery as #17's classification logic,
//! it's backend-specific SQL in the reference's `SqliteStorage.
//! _insert_revision`. It's tracked as part of #36, not a separate engine
//! issue. #19 is effectively subsumed rather than implemented here.

use std::collections::{HashMap, HashSet};

use serde_json::json;

use crate::capabilities::Capabilities;
use crate::errors::{ConnectorError, DbsError};
use crate::hashing::content_hash;
use crate::models::{BackupItem, Checkpoint};
use crate::storage::{BatchResult, PreparedItem, Storage};
use crate::timeutil::iso_z;

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

/// Converts a connector-emitted [`BackupItem`] into a [`PreparedItem`]
/// ready for a storage backend to classify and persist.
///
/// Errors with [`ConnectorError::Contract`] if `item.item_kind` isn't one
/// of `valid_kinds` — a connector emitting an undeclared kind is a
/// programming error, surfaced loudly rather than silently accepted.
pub fn prepare(
    item: &BackupItem,
    capabilities: &Capabilities,
    volatile_fields: &[String],
    valid_kinds: &[String],
) -> Result<PreparedItem, ConnectorError> {
    if !valid_kinds.iter().any(|k| k == &item.item_kind) {
        return Err(ConnectorError::Contract(format!(
            "item_kind {:?} (id={:?}) is not in the connector's declared item_kinds {valid_kinds:?}",
            item.item_kind,
            item.external_id(),
        )));
    }
    let deleted = item.deleted && capabilities.supports_native_deletes;
    let content_hash = compute_hash(item, volatile_fields, deleted);
    let media = if capabilities.produces_media {
        item.media
            .iter()
            .map(|m| serde_json::to_value(m).expect("MediaRef always serializes"))
            .collect()
    } else {
        Vec::new()
    };
    Ok(PreparedItem {
        external_id: item.external_id().to_string(),
        item_kind: item.item_kind.clone(),
        title: item.title.clone(),
        url: item.url.clone(),
        body: item.body.clone(),
        tags: item.tags.clone(),
        item_created_at: item.created_at.map(iso_z),
        item_updated_at: item.updated_at.map(iso_z),
        content_hash,
        raw_json: item.raw.to_string(),
        deleted,
        media,
    })
}

/// A `revision_token`, when the connector supplies one, is the change
/// signal on its own (an etag/version the upstream API already
/// guarantees is stable) — the normalized-projection hash below is
/// skipped entirely rather than computed and ignored.
fn compute_hash(item: &BackupItem, volatile_fields: &[String], deleted: bool) -> String {
    if let Some(token) = &item.revision_token {
        return content_hash(&json!({ "revision_token": token }));
    }
    let mut raw_clean = item.raw.clone();
    if let Some(obj) = raw_clean.as_object_mut() {
        for key in volatile_fields {
            obj.remove(key);
        }
    }
    let mut tags_sorted = item.tags.clone();
    tags_sorted.sort();
    let projection = json!({
        "item_kind": item.item_kind,
        "title": item.title,
        "url": item.url,
        "body": item.body,
        "tags": tags_sorted,
        "deleted": deleted,
        "raw": raw_clean,
    });
    content_hash(&projection)
}

/// Outcome of one [`sweep_deletions`] call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepOutcome {
    pub deleted: u64,
    pub revisions: u64,
    /// One entry per scope that was skipped (unrecognized) or refused
    /// (unsafe) — never silent, matching the reference's behavior of
    /// always surfacing a sweep refusal as a run warning.
    pub warnings: Vec<String>,
}

/// Decides, per reconcile scope, whether it's safe to soft-delete items
/// missing from a full enumeration — and does so when it is.
///
/// A scope is refused (a warning is recorded, no delete happens) when
/// the enumeration is empty while storage still has live items, or when
/// the fraction of live items that would be deleted exceeds
/// `sweep_safety_fraction` — both are the signature of a truncated
/// upstream listing, not genuine mass deletion. `reconcile_scopes` maps
/// scope name (`"source"` or `"tag:<value>"`) to the full set of live
/// external ids the connector enumerated for that scope; an
/// unrecognized scope shape is refused rather than silently widened
/// into a source-wide sweep.
pub fn sweep_deletions(
    storage: &mut dyn Storage,
    source_id: i64,
    run_id: i64,
    reconcile_scopes: &HashMap<String, HashSet<String>>,
    sweep_safety_fraction: f64,
) -> Result<SweepOutcome, DbsError> {
    let mut outcome = SweepOutcome::default();
    for (scope, live) in reconcile_scopes {
        let tag: Option<&str> = if scope == "source" {
            None
        } else if let Some(t) = scope.strip_prefix("tag:") {
            Some(t)
        } else {
            outcome.warnings.push(format!(
                "deletion sweep skipped for unrecognized reconcile scope {scope:?}"
            ));
            continue;
        };

        let existing_live = storage.live_external_ids(source_id, tag)?;
        let would_delete = existing_live.difference(live).count();
        let n_live = existing_live.len();
        let fraction = if n_live > 0 {
            would_delete as f64 / n_live as f64
        } else {
            0.0
        };
        let unsafe_sweep = n_live > 0 && (live.is_empty() || fraction > sweep_safety_fraction);
        if unsafe_sweep {
            let where_ = if tag.is_some() {
                format!(" within {scope:?}")
            } else {
                String::new()
            };
            outcome.warnings.push(format!(
                "deletion sweep skipped for safety{where_}: enumeration would delete \
                 {would_delete}/{n_live} live items ({:.0}% > {:.0}%); the upstream \
                 listing looks incomplete",
                fraction * 100.0,
                sweep_safety_fraction * 100.0
            ));
            continue;
        }

        let swept = storage.soft_delete_missing(source_id, live, run_id, tag)?;
        outcome.deleted += swept;
        outcome.revisions += swept;
    }
    Ok(outcome)
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

    mod prepare_tests {
        use super::super::*;
        use crate::models::BackupItem;

        fn backup_item(raw: serde_json::Value) -> BackupItem {
            BackupItem::new("ext-1", "post", raw).unwrap()
        }

        #[test]
        fn rejects_an_undeclared_item_kind() {
            let item = backup_item(json!({}));
            let err = prepare(
                &item,
                &Capabilities::default(),
                &[],
                &["comment".to_string()],
            )
            .unwrap_err();
            assert!(matches!(err, ConnectorError::Contract(_)));
        }

        #[test]
        fn accepts_a_declared_item_kind() {
            let item = backup_item(json!({"a": 1}));
            let prepared =
                prepare(&item, &Capabilities::default(), &[], &["post".to_string()]).unwrap();
            assert_eq!(prepared.external_id, "ext-1");
            assert_eq!(prepared.item_kind, "post");
        }

        #[test]
        fn deleted_is_false_unless_capability_declares_native_deletes() {
            let mut item = backup_item(json!({}));
            item.deleted = true;
            let no_native_deletes = Capabilities::default();
            let prepared = prepare(&item, &no_native_deletes, &[], &["post".to_string()]).unwrap();
            assert!(!prepared.deleted);

            let with_native_deletes = Capabilities {
                supports_native_deletes: true,
                ..Capabilities::default()
            };
            let prepared =
                prepare(&item, &with_native_deletes, &[], &["post".to_string()]).unwrap();
            assert!(prepared.deleted);
        }

        #[test]
        fn media_is_empty_unless_capability_declares_produces_media() {
            let mut item = backup_item(json!({}));
            item.media = vec![crate::models::MediaRef::new("https://example.com/x.png")];

            let no_media = Capabilities::default();
            let prepared = prepare(&item, &no_media, &[], &["post".to_string()]).unwrap();
            assert!(prepared.media.is_empty());

            let with_media = Capabilities {
                produces_media: true,
                ..Capabilities::default()
            };
            let prepared = prepare(&item, &with_media, &[], &["post".to_string()]).unwrap();
            assert_eq!(prepared.media.len(), 1);
        }

        #[test]
        fn revision_token_shortcuts_the_projection_hash() {
            let mut a = backup_item(json!({"noisy": "value-a"}));
            a.revision_token = Some("v1".to_string());
            let mut b = backup_item(json!({"noisy": "value-b"}));
            b.revision_token = Some("v1".to_string());
            // Wildly different raw payloads, same revision_token -> same hash.
            let hash_a = compute_hash(&a, &[], false);
            let hash_b = compute_hash(&b, &[], false);
            assert_eq!(hash_a, hash_b);
        }

        #[test]
        fn different_revision_tokens_hash_differently() {
            let mut a = backup_item(json!({}));
            a.revision_token = Some("v1".to_string());
            let mut b = backup_item(json!({}));
            b.revision_token = Some("v2".to_string());
            assert_ne!(compute_hash(&a, &[], false), compute_hash(&b, &[], false));
        }

        #[test]
        fn volatile_fields_are_stripped_before_hashing() {
            let a = backup_item(json!({"stable": "x", "fetched_at": "t1"}));
            let b = backup_item(json!({"stable": "x", "fetched_at": "t2"}));
            let volatile = vec!["fetched_at".to_string()];
            assert_eq!(
                compute_hash(&a, &volatile, false),
                compute_hash(&b, &volatile, false)
            );
            // Without stripping, the two would hash differently.
            assert_ne!(compute_hash(&a, &[], false), compute_hash(&b, &[], false));
        }

        #[test]
        fn tag_order_does_not_affect_the_hash() {
            let mut a = backup_item(json!({}));
            a.tags = vec!["b".to_string(), "a".to_string()];
            let mut b = backup_item(json!({}));
            b.tags = vec!["a".to_string(), "b".to_string()];
            assert_eq!(compute_hash(&a, &[], false), compute_hash(&b, &[], false));
        }

        #[test]
        fn deleted_flag_participates_in_the_hash() {
            let item = backup_item(json!({}));
            assert_ne!(
                compute_hash(&item, &[], true),
                compute_hash(&item, &[], false)
            );
        }

        #[test]
        fn timestamps_are_formatted_via_iso_z() {
            use chrono::{TimeZone, Utc};
            let mut item = backup_item(json!({}));
            item.created_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
            let prepared =
                prepare(&item, &Capabilities::default(), &[], &["post".to_string()]).unwrap();
            assert_eq!(
                prepared.item_created_at.as_deref(),
                Some("2026-01-01T00:00:00Z")
            );
            assert_eq!(prepared.item_updated_at, None);
        }
    }

    mod sweep_tests {
        use super::super::*;
        use crate::models::Cursor;
        use crate::storage::{ExportQuery, ItemRow, SourceRecord};
        use chrono::{DateTime, Utc};

        /// Tracks `live_external_ids`/`soft_delete_missing` calls against
        /// a fixed "existing live" set per (source, tag) pair, so tests
        /// can control exactly what a sweep decision sees.
        #[derive(Default)]
        struct SweepStorage {
            /// Keyed by tag ("" for the source-wide scope).
            existing_live: HashMap<String, HashSet<String>>,
            delete_calls: Vec<(Option<String>, usize)>,
        }

        impl SweepStorage {
            fn with_live(tag: &str, ids: &[&str]) -> Self {
                let mut s = Self::default();
                s.existing_live
                    .insert(tag.to_string(), ids.iter().map(|s| s.to_string()).collect());
                s
            }
        }

        impl Storage for SweepStorage {
            fn migrate(&mut self) -> Result<(), DbsError> {
                unimplemented!()
            }
            fn close(&mut self) {}
            fn upsert_source(
                &mut self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
                _: u32,
            ) -> Result<SourceRecord, DbsError> {
                unimplemented!()
            }
            fn get_source(&self, _: &str) -> Result<Option<SourceRecord>, DbsError> {
                unimplemented!()
            }
            fn list_sources(&self) -> Result<Vec<SourceRecord>, DbsError> {
                unimplemented!()
            }
            fn delete_source(&mut self, _: &str) -> Result<bool, DbsError> {
                unimplemented!()
            }
            fn begin_run(
                &mut self,
                _: i64,
                _: &str,
                _: &str,
                _: Option<&str>,
            ) -> Result<i64, DbsError> {
                unimplemented!()
            }
            fn finish_run(
                &mut self,
                _: i64,
                _: &str,
                _: &BatchResult,
                _: u64,
                _: Option<&str>,
                _: Option<&str>,
                _: &[String],
            ) -> Result<(), DbsError> {
                unimplemented!()
            }
            fn reap_interrupted_runs(&mut self) -> Result<Vec<i64>, DbsError> {
                unimplemented!()
            }
            fn recent_runs(&self, _: Option<i64>, _: u32) -> Result<Vec<ItemRow>, DbsError> {
                unimplemented!()
            }
            fn upsert_items(
                &mut self,
                _: i64,
                _: i64,
                _: &[PreparedItem],
                _: bool,
                _: u64,
            ) -> Result<BatchResult, DbsError> {
                unimplemented!()
            }
            fn soft_delete_missing(
                &mut self,
                _source_id: i64,
                live_ids: &HashSet<String>,
                _run_id: i64,
                tag: Option<&str>,
            ) -> Result<u64, DbsError> {
                let key = tag.unwrap_or("").to_string();
                let existing = self.existing_live.get(&key).cloned().unwrap_or_default();
                let swept = existing.difference(live_ids).count();
                self.delete_calls.push((tag.map(str::to_string), swept));
                Ok(swept as u64)
            }
            fn live_external_ids(
                &self,
                _source_id: i64,
                tag: Option<&str>,
            ) -> Result<HashSet<String>, DbsError> {
                let key = tag.unwrap_or("").to_string();
                Ok(self.existing_live.get(&key).cloned().unwrap_or_default())
            }
            fn save_cursor(
                &mut self,
                _: i64,
                _: Option<&Cursor>,
                _: Option<&str>,
                _: i64,
            ) -> Result<(), DbsError> {
                unimplemented!()
            }
            fn load_cursor(
                &self,
                _: i64,
            ) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError> {
                unimplemented!()
            }
            fn get_run_count(&self, _: i64) -> Result<u64, DbsError> {
                unimplemented!()
            }
            fn increment_run_count(&mut self, _: i64) -> Result<(), DbsError> {
                unimplemented!()
            }
            fn acquire_lock(&mut self, _: i64, _: i64) -> Result<bool, DbsError> {
                unimplemented!()
            }
            fn release_lock(&mut self, _: i64) -> Result<(), DbsError> {
                unimplemented!()
            }
            fn iter_items<'a>(
                &'a self,
                _: &ExportQuery,
            ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
                unimplemented!()
            }
            fn iter_revisions<'a>(
                &'a self,
                _: &ExportQuery,
            ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
                unimplemented!()
            }
            fn item_counts(&self, _: i64) -> Result<(u64, u64, u64), DbsError> {
                unimplemented!()
            }
            fn browse_items(
                &self,
                _: &ExportQuery,
                _: Option<&str>,
                _: u32,
                _: u32,
            ) -> Result<(Vec<ItemRow>, u64), DbsError> {
                unimplemented!()
            }
            fn get_item(&self, _: i64) -> Result<Option<ItemRow>, DbsError> {
                unimplemented!()
            }
            fn get_media_blob(&self, _: i64) -> Result<Option<ItemRow>, DbsError> {
                unimplemented!()
            }
            fn metrics(&self) -> Result<ItemRow, DbsError> {
                unimplemented!()
            }
            fn integrity_check(&self) -> Result<String, DbsError> {
                unimplemented!()
            }
        }

        fn ids(items: &[&str]) -> HashSet<String> {
            items.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn unrecognized_scope_is_skipped_with_a_warning_and_no_delete() {
            let mut storage = SweepStorage::default();
            let mut scopes = HashMap::new();
            scopes.insert("bogus-scope".to_string(), ids(&["1"]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 0);
            assert_eq!(outcome.warnings.len(), 1);
            assert!(outcome.warnings[0].contains("unrecognized"));
            assert!(storage.delete_calls.is_empty());
        }

        #[test]
        fn safe_source_scope_sweep_deletes_missing_items() {
            // Existing live: 1,2,3,4. Enumeration: 1,2,3 (only "4" missing
            // -> 1/4 = 25%, under the 50% safety fraction).
            let mut storage = SweepStorage::with_live("", &["1", "2", "3", "4"]);
            let mut scopes = HashMap::new();
            scopes.insert("source".to_string(), ids(&["1", "2", "3"]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 1);
            assert_eq!(outcome.revisions, 1);
            assert!(outcome.warnings.is_empty());
            assert_eq!(storage.delete_calls, vec![(None, 1)]);
        }

        #[test]
        fn tag_scope_passes_the_tag_through_to_storage() {
            let mut storage = SweepStorage::with_live("topic-a", &["1", "2"]);
            let mut scopes = HashMap::new();
            scopes.insert("tag:topic-a".to_string(), ids(&["1", "2"]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 0);
            assert_eq!(storage.delete_calls, vec![(Some("topic-a".to_string()), 0)]);
        }

        #[test]
        fn empty_enumeration_against_existing_live_items_is_refused() {
            let mut storage = SweepStorage::with_live("", &["1", "2"]);
            let mut scopes = HashMap::new();
            scopes.insert("source".to_string(), ids(&[]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 0);
            assert_eq!(outcome.warnings.len(), 1);
            assert!(outcome.warnings[0].contains("safety"));
            assert!(storage.delete_calls.is_empty());
        }

        #[test]
        fn fraction_over_the_safety_threshold_is_refused() {
            // 4 live, enumeration has only 1 -> 3/4 = 75% would be deleted,
            // over the 50% threshold.
            let mut storage = SweepStorage::with_live("", &["1", "2", "3", "4"]);
            let mut scopes = HashMap::new();
            scopes.insert("source".to_string(), ids(&["1"]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 0);
            assert!(outcome.warnings[0].contains("75%"));
            assert!(storage.delete_calls.is_empty());
        }

        #[test]
        fn fraction_exactly_at_the_threshold_is_allowed() {
            // 4 live, enumeration missing exactly 2 -> 2/4 = 50%, not
            // strictly over a 50% threshold.
            let mut storage = SweepStorage::with_live("", &["1", "2", "3", "4"]);
            let mut scopes = HashMap::new();
            scopes.insert("source".to_string(), ids(&["1", "2"]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 2);
            assert!(outcome.warnings.is_empty());
        }

        #[test]
        fn no_existing_live_items_is_never_unsafe() {
            // Nothing to accidentally mass-delete.
            let mut storage = SweepStorage::with_live("", &[]);
            let mut scopes = HashMap::new();
            scopes.insert("source".to_string(), ids(&[]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 0);
            assert!(outcome.warnings.is_empty());
        }

        #[test]
        fn multiple_scopes_are_each_evaluated_independently() {
            let mut storage = SweepStorage::default();
            storage
                .existing_live
                .insert("".to_string(), ids(&["1", "2"]));
            storage
                .existing_live
                .insert("a".to_string(), ids(&["3", "4"]));
            let mut scopes = HashMap::new();
            // "source" scope: safe (nothing missing).
            scopes.insert("source".to_string(), ids(&["1", "2"]));
            // "tag:a" scope: unsafe (100% would be deleted).
            scopes.insert("tag:a".to_string(), ids(&[]));
            let outcome = sweep_deletions(&mut storage, 1, 1, &scopes, 0.5).unwrap();
            assert_eq!(outcome.deleted, 0);
            assert_eq!(outcome.warnings.len(), 1);
        }
    }
}
