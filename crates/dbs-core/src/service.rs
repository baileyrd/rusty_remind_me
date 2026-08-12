//! `BackupService`-level orchestration — the UI-agnostic façade the CLI
//! and (eventually) a web tier both render over.
//!
//! Mirrors `src/dbs/core/service.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`), a module `gap-analysis.md` missed entirely in
//! its original pass (same failure class as `export_profile.py` — found
//! only once something needed it). **This module's scope so far:** just
//! the crash-recovery reap-once guarantee (#21) — `BackupService` itself
//! is much larger (connector instantiation via the registry, VPN guard
//! checks, `backup_source`/`backup_all` orchestration, status/history
//! rendering) and needs its own follow-up issue(s), noted in
//! `gap-analysis.md`.
//!
//! `reap_interrupted_runs()` must run *exactly once* per top-level
//! service call — once before a standalone `backup_source`, or once
//! before an entire `backup_all` batch, never once per source touched
//! within that batch. The reference's docstring is explicit about why:
//! "a per-source reap inside a parallel batch would flip a sibling's
//! genuinely-running row" — `backup --all --parallel N` has concurrent
//! workers, and a mid-batch reap could incorrectly interrupt a source
//! whose run legitimately started after the batch began but before that
//! particular reap call.

use crate::errors::DbsError;
use crate::storage::Storage;

/// Calls `storage.reap_interrupted_runs()` unless `already_reaped` is
/// already `true`, then sets it — so repeated calls sharing the same
/// flag across a batch collapse to a single reap. Returns the ids of
/// runs that were flipped to `interrupted` (empty if this call was a
/// no-op because reaping already happened).
pub fn reap_once(
    storage: &mut dyn Storage,
    already_reaped: &mut bool,
) -> Result<Vec<i64>, DbsError> {
    if *already_reaped {
        return Ok(Vec::new());
    }
    let reaped = storage.reap_interrupted_runs()?;
    *already_reaped = true;
    Ok(reaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Cursor;
    use crate::storage::{BatchResult, ExportQuery, ItemRow, PreparedItem, SourceRecord};
    use chrono::{DateTime, Utc};
    use std::collections::HashSet;

    /// Counts `reap_interrupted_runs` calls; every other method is
    /// unreachable by this issue's tests.
    #[derive(Default)]
    struct CountingStorage {
        reap_calls: usize,
    }

    impl Storage for CountingStorage {
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
            self.reap_calls += 1;
            Ok(vec![self.reap_calls as i64])
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
            _: i64,
            _: &HashSet<String>,
            _: i64,
            _: Option<&str>,
        ) -> Result<u64, DbsError> {
            unimplemented!()
        }
        fn live_external_ids(&self, _: i64, _: Option<&str>) -> Result<HashSet<String>, DbsError> {
            unimplemented!()
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
        fn load_cursor(&self, _: i64) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError> {
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

    #[test]
    fn first_call_reaps() {
        let mut storage = CountingStorage::default();
        let mut already_reaped = false;
        let reaped = reap_once(&mut storage, &mut already_reaped).unwrap();
        assert_eq!(reaped, vec![1]);
        assert_eq!(storage.reap_calls, 1);
        assert!(already_reaped);
    }

    #[test]
    fn repeated_calls_sharing_the_flag_reap_only_once() {
        let mut storage = CountingStorage::default();
        let mut already_reaped = false;
        // Simulates backup_all touching 3 sources with one shared flag.
        for _ in 0..3 {
            reap_once(&mut storage, &mut already_reaped).unwrap();
        }
        assert_eq!(storage.reap_calls, 1);
    }

    #[test]
    fn independent_flags_each_reap_once() {
        // Simulates two standalone backup_source calls, each with its
        // own _reap=true default — neither shares the other's flag.
        let mut storage = CountingStorage::default();
        let mut first_call_flag = false;
        let mut second_call_flag = false;
        reap_once(&mut storage, &mut first_call_flag).unwrap();
        reap_once(&mut storage, &mut second_call_flag).unwrap();
        assert_eq!(storage.reap_calls, 2);
    }
}
