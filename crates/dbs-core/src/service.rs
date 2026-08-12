//! `BackupService`-level orchestration — the UI-agnostic façade the CLI
//! and (eventually) a web tier both render over.
//!
//! Mirrors `src/dbs/core/service.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`), a module `gap-analysis.md` missed entirely in
//! its original pass (same failure class as `export_profile.py` — found
//! only once something needed it). Landed: the crash-recovery reap-once
//! guarantee (#21), and — this issue (#46) — connector instantiation via
//! the plugin registry, VPN guard checks, run-mode selection,
//! `backup_source`/`backup_all` orchestration, and status/history
//! rendering.
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
//!
//! **Scope note — the [`ConnectorRunner`] seam.** The reference's
//! `backup_source` hands off to `self.engine.run_source(rc, ctx, ...)`,
//! which drives the connector's actual fetch loop (spawn/handshake was
//! #45; writing a `RunContext` and reading a `FetchEvent` stream back —
//! ADR-0001 steps 2-3 — is separate follow-up work no issue covers yet).
//! Rather than block this issue's connector-instantiation/VPN-guard/
//! batching scope on that follow-up landing first, [`ConnectorRunner`]
//! is the injected seam the reference's constructor-injected `engine`
//! plays: `BackupService` does every preflight step for real (registry
//! lookup, source registration, cursor/run-count load, mode selection,
//! locking, run bookkeeping) and calls out to a `&dyn ConnectorRunner`
//! for the actual fetch. [`UnimplementedRunner`] is the production
//! stand-in until that follow-up issue supplies a real one; tests use a
//! scripted fake. This is a **deliberate improvement**, not just a
//! stopgap: unlike the reference (an uncaught exception from
//! `engine.run_source` skips `finish_run` entirely, leaving the row
//! `running` until the next reap), `backup_source` here always calls
//! `finish_run` exactly once, translating a runner error into a `Failed`
//! result instead.

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::capabilities::Capabilities;
use crate::config::{Config, SourceConfig, VpnGuard};
use crate::errors::{BackupRunError, ConnectorError, ConnectorLoadError, DbsError};
use crate::models::{Cursor, RunResult, RunStatus, SourceStatus};
use crate::netns::in_named_netns;
use crate::registry::{ConnectorRegistry, RegisteredConnector};
use crate::storage::{BatchResult, ItemRow, Storage};
use crate::timeutil::parse_iso;

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

/// Reconcile cadence fallback when a source declares none — mirrors the
/// reference's `_DEFAULT_RECONCILE_EVERY`.
const DEFAULT_RECONCILE_EVERY: u32 = 7;

/// Per-cadence "due again after" window, deliberately short of its
/// nominal period (daily -> 20h, not 24h) so a timer firing at slightly
/// varying times (cron drift, a laptop waking late) never skips a whole
/// period. Unknown/missing schedules fall back to daily, same as the
/// reference.
fn schedule_slack(schedule: &str) -> ChronoDuration {
    match schedule.to_ascii_lowercase().as_str() {
        "hourly" => ChronoDuration::minutes(50),
        "weekly" => ChronoDuration::days(6),
        _ => ChronoDuration::hours(20),
    }
}

/// `None` means due right now (never run, or unreadable history) —
/// mirrors the reference's `_next_due_at`.
fn next_due_at(last_started: Option<DateTime<Utc>>, schedule: &str) -> Option<DateTime<Utc>> {
    last_started.map(|last| last + schedule_slack(schedule))
}

fn is_due(last_started: Option<DateTime<Utc>>, schedule: &str, now: DateTime<Utc>) -> bool {
    match next_due_at(last_started, schedule) {
        Some(due) => now >= due,
        None => true,
    }
}

/// Picks a run mode, mirroring the reference's `_choose_mode` exactly:
/// an explicit `force_full` always wins; a connector that can't do
/// incremental fetches always runs full; `force_reconcile` only applies
/// when the connector supports full enumeration; a first-ever run (no
/// cursor) is full when the connector can fully enumerate, else
/// incremental; an explicit `mode` of incremental/reconcile/full is
/// honored (downgrading an unsupported `reconcile` to `incremental`);
/// otherwise ("auto") reconciles every `reconcile_every_runs` runs.
#[allow(clippy::too_many_arguments)]
fn choose_mode(
    mode: &str,
    force_full: bool,
    force_reconcile: bool,
    cursor: Option<&Cursor>,
    run_count: u64,
    reconcile_every_runs: Option<u32>,
    caps: &Capabilities,
) -> String {
    if force_full || !caps.supports_incremental {
        return "full".to_string();
    }
    if force_reconcile && caps.supports_full_enumeration {
        return "reconcile".to_string();
    }
    if cursor.is_none() {
        return if caps.supports_full_enumeration {
            "full"
        } else {
            "incremental"
        }
        .to_string();
    }
    if mode == "incremental" || mode == "reconcile" || mode == "full" {
        if mode == "reconcile" && !caps.supports_full_enumeration {
            return "incremental".to_string();
        }
        return mode.to_string();
    }
    // "auto"
    let every = reconcile_every_runs
        .filter(|&e| e > 0)
        .unwrap_or(DEFAULT_RECONCILE_EVERY) as u64;
    if caps.supports_full_enumeration && run_count.is_multiple_of(every) {
        "reconcile".to_string()
    } else {
        "incremental".to_string()
    }
}

/// Refuses a `requires_vpn` source launched outside `config.vpn_netns`
/// (the recorded failure mode for IP-blocked connectors: an off-VPN run
/// silently exposes the host IP). `vpn_guard` downgrades this to
/// proceed-anyway (`Warn`) or disables it (`Off`); already being inside
/// the namespace always proceeds. Returns `Some(Skipped result)` to
/// abort the run, `None` to proceed — mirrors the reference's
/// `_vpn_guard_skip`. Logging the warn-path message is deferred (this
/// crate has no logging framework wired in yet).
fn vpn_guard_skip(
    config: &Config,
    name: &str,
    sc: &SourceConfig,
    now: DateTime<Utc>,
) -> Option<RunResult> {
    if !sc.requires_vpn || config.vpn_guard == VpnGuard::Off {
        return None;
    }
    if in_named_netns(&config.vpn_netns) {
        return None;
    }
    if config.vpn_guard == VpnGuard::Warn {
        return None;
    }
    let msg = format!(
        "{name} is marked requires_vpn but this process is not in the {:?} network \
         namespace — run it through the VPN wrapper, e.g. `{} dbs backup {name}`",
        config.vpn_netns, config.vpn_exec
    );
    Some(RunResult::skipped(name, now, msg))
}

fn run_status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "success",
        RunStatus::Partial => "partial",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
        RunStatus::Interrupted => "interrupted",
    }
}

/// What a [`ConnectorRunner`] reports back from actually running a
/// connector's fetch — everything `BackupService` needs to call
/// `Storage::finish_run` and build the returned [`RunResult`].
#[derive(Debug, Clone, Default)]
pub struct ConnectorRunOutcome {
    pub status: RunStatus,
    pub stats: BatchResult,
    pub items_seen: u64,
    pub cursor_after: Option<Cursor>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

/// Drives one connector's fetch given a resolved run — the seam that
/// stands in for the reference's `Engine.run_source` until ADR-0001's
/// run/stream protocol (steps 2-3) has an implementation. See the
/// module doc-comment's scope note.
pub trait ConnectorRunner {
    #[allow(clippy::too_many_arguments)]
    fn run_connector(
        &self,
        connector: &RegisteredConnector,
        run_id: i64,
        source_id: i64,
        mode: &str,
        cursor: Option<&Cursor>,
        since: Option<DateTime<Utc>>,
    ) -> Result<ConnectorRunOutcome, DbsError>;
}

/// Production stand-in for [`ConnectorRunner`] until the connector
/// run/stream bridge exists — every call fails clearly (a `Failed`
/// [`RunResult`] with an explanatory message) instead of silently
/// returning bogus data.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnimplementedRunner;

impl ConnectorRunner for UnimplementedRunner {
    fn run_connector(
        &self,
        connector: &RegisteredConnector,
        _run_id: i64,
        _source_id: i64,
        _mode: &str,
        _cursor: Option<&Cursor>,
        _since: Option<DateTime<Utc>>,
    ) -> Result<ConnectorRunOutcome, DbsError> {
        Err(DbsError::Connector(ConnectorError::Contract(format!(
            "connector run/stream protocol not implemented yet — cannot run \
             {:?} (ADR-0001 steps 2-3, follow-up to issue #45)",
            connector.type_
        ))))
    }
}

/// Options for [`BackupService::backup_source`]. `mode` is one of
/// `"auto"` (default), `"incremental"`, `"reconcile"`, or `"full"`.
#[derive(Debug, Clone)]
pub struct BackupSourceOptions {
    pub mode: String,
    pub force_full: bool,
    pub force_reconcile: bool,
    pub dry_run: bool,
    pub limit: Option<u32>,
    /// Whether this call should itself reap interrupted runs — `false`
    /// when called from `backup_all`, which reaps once up front instead.
    pub reap: bool,
}

impl Default for BackupSourceOptions {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            force_full: false,
            force_reconcile: false,
            dry_run: false,
            limit: None,
            reap: true,
        }
    }
}

/// Options for [`BackupService::backup_all`]. `--parallel N` and
/// `--only-due` are separate filed issues — this always runs every
/// enabled source sequentially, in name-sorted order (the reference
/// preserves TOML declaration order via a Python dict; this crate's
/// `Config::sources` is a `HashMap`, so sorted-by-name is the
/// deterministic substitute).
#[derive(Debug, Clone)]
pub struct BackupAllOptions {
    pub continue_on_error: bool,
    pub force_full: bool,
    pub force_reconcile: bool,
    pub dry_run: bool,
    pub limit: Option<u32>,
}

impl Default for BackupAllOptions {
    fn default() -> Self {
        Self {
            continue_on_error: true,
            force_full: false,
            force_reconcile: false,
            dry_run: false,
            limit: None,
        }
    }
}

/// The UI-agnostic application core the CLI (and, eventually, a web
/// tier) both render over. See the module doc-comment for what this
/// issue covers and the [`ConnectorRunner`] seam it introduces.
pub struct BackupService<'a> {
    pub storage: &'a mut dyn Storage,
    pub config: &'a Config,
    pub registry: &'a ConnectorRegistry,
    pub runner: &'a dyn ConnectorRunner,
}

impl<'a> BackupService<'a> {
    pub fn new(
        storage: &'a mut dyn Storage,
        config: &'a Config,
        registry: &'a ConnectorRegistry,
        runner: &'a dyn ConnectorRunner,
    ) -> Self {
        Self {
            storage,
            config,
            registry,
            runner,
        }
    }

    /// Backs up one source end to end: reap (if requested), source
    /// lookup, disabled/VPN-guard early exits, connector instantiation
    /// via the registry, cursor/run-count load, mode selection, run
    /// bookkeeping (`begin_run`/lock/`finish_run`), and handing off to
    /// [`ConnectorRunner`]. Mirrors the reference's `backup_source`.
    pub fn backup_source(
        &mut self,
        name: &str,
        opts: &BackupSourceOptions,
    ) -> Result<RunResult, DbsError> {
        if opts.reap {
            self.storage.reap_interrupted_runs()?;
        }
        let sc = self
            .config
            .sources
            .get(name)
            .ok_or_else(|| DbsError::Run(BackupRunError::UnknownSource(name.to_string())))?
            .clone();
        let now = Utc::now();
        if !sc.enabled {
            return Ok(RunResult::skipped(name, now, "source disabled"));
        }

        if let Some(skip) = vpn_guard_skip(self.config, name, &sc, now) {
            return Ok(skip);
        }

        let rc = self
            .registry
            .get(&sc.type_)
            .cloned()
            .ok_or_else(|| DbsError::Load(ConnectorLoadError::NotFound(sc.type_.clone())))?;

        let config_json = serde_json::to_string(&sc.options)
            .map_err(|e| DbsError::Config(format!("failed to encode source options: {e}")))?;
        let source = self.storage.upsert_source(
            name,
            &sc.type_,
            &rc.plugin_id,
            &config_json,
            rc.handshake.schema_version,
        )?;
        let (cursor, watermark) = self.storage.load_cursor(source.id)?;
        let run_count = self.storage.get_run_count(source.id)?;
        let chosen_mode = choose_mode(
            &opts.mode,
            opts.force_full,
            opts.force_reconcile,
            cursor.as_ref(),
            run_count,
            sc.reconcile_every_runs,
            &rc.handshake.capabilities,
        );

        if opts.dry_run {
            return Ok(RunResult {
                mode: chosen_mode,
                ..RunResult::skipped(name, now, "dry-run")
            });
        }

        let cursor_before = cursor
            .as_ref()
            .map(|c| serde_json::to_string(&c.value))
            .transpose()
            .map_err(|e| DbsError::Config(format!("failed to encode cursor: {e}")))?;
        let run_id = self.storage.begin_run(
            source.id,
            &rc.plugin_id,
            &chosen_mode,
            cursor_before.as_deref(),
        )?;
        if !self.storage.acquire_lock(source.id, run_id)? {
            self.storage.finish_run(
                run_id,
                run_status_str(RunStatus::Skipped),
                &BatchResult::default(),
                0,
                cursor_before.as_deref(),
                Some("source locked"),
                &[],
            )?;
            return Err(DbsError::Run(BackupRunError::SourceLocked(
                name.to_string(),
            )));
        }

        let since = watermark
            .map(|w| w - ChronoDuration::seconds(self.config.default_overlap_seconds as i64));
        let outcome =
            self.runner
                .run_connector(&rc, run_id, source.id, &chosen_mode, cursor.as_ref(), since);
        let finished_at = Utc::now();

        // Each cleanup step is best-effort and independent, mirroring
        // the reference's `finally` block — one failure can't mask the
        // others or the run result.
        let _ = self.storage.release_lock(source.id);
        let _ = self.storage.increment_run_count(source.id);

        let outcome = outcome.unwrap_or_else(|e| ConnectorRunOutcome {
            status: RunStatus::Failed,
            error: Some(e.to_string()),
            ..ConnectorRunOutcome::default()
        });
        let cursor_after_json = outcome
            .cursor_after
            .as_ref()
            .map(|c| serde_json::to_string(&c.value))
            .transpose()
            .map_err(|e| DbsError::Config(format!("failed to encode cursor: {e}")))?;
        self.storage.finish_run(
            run_id,
            run_status_str(outcome.status),
            &outcome.stats,
            outcome.items_seen,
            cursor_after_json.as_deref(),
            outcome.error.as_deref(),
            &outcome.warnings,
        )?;

        Ok(RunResult {
            source: name.to_string(),
            status: outcome.status,
            started_at: now,
            finished_at,
            mode: chosen_mode,
            run_id: Some(run_id),
            fetched: outcome.items_seen,
            created: outcome.stats.created,
            updated: outcome.stats.updated,
            unchanged: outcome.stats.unchanged,
            deleted: outcome.stats.deleted,
            undeleted: outcome.stats.undeleted,
            revisions: outcome.stats.revisions,
            items_failed: 0,
            error: outcome.error,
            warnings: outcome.warnings,
        })
    }

    /// Backs up every enabled source sequentially (see
    /// [`BackupAllOptions`] for what's deferred to other issues).
    /// Reaps once, up front — a per-source reap mid-batch would flip a
    /// sibling's genuinely-running row once `--parallel N` lands.
    pub fn backup_all(&mut self, opts: &BackupAllOptions) -> Result<Vec<RunResult>, DbsError> {
        self.storage.reap_interrupted_runs()?;

        let mut names: Vec<String> = self
            .config
            .sources
            .iter()
            .filter(|(_, sc)| sc.enabled)
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();

        let per_source = BackupSourceOptions {
            mode: "auto".to_string(),
            force_full: opts.force_full,
            force_reconcile: opts.force_reconcile,
            dry_run: opts.dry_run,
            limit: opts.limit,
            reap: false,
        };

        let mut results = Vec::with_capacity(names.len());
        for name in &names {
            match self.backup_source(name, &per_source) {
                Ok(r) => results.push(r),
                Err(e) => {
                    if !opts.continue_on_error {
                        return Err(e);
                    }
                    results.push(RunResult::failed(name, Utc::now(), e.to_string()));
                }
            }
        }
        Ok(results)
    }

    /// One [`SourceStatus`] per named source (every configured source if
    /// `name` is `None`, sorted by name — see [`BackupAllOptions`]'s doc
    /// on `HashMap` iteration order).
    pub fn status(&self, name: Option<&str>) -> Result<Vec<SourceStatus>, DbsError> {
        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => {
                let mut v: Vec<String> = self.config.sources.keys().cloned().collect();
                v.sort();
                v
            }
        };
        let now = Utc::now();
        let mut out = Vec::with_capacity(names.len());
        for n in &names {
            let sc = self.config.sources.get(n);
            let type_ = sc
                .map(|s| s.type_.clone())
                .unwrap_or_else(|| "?".to_string());
            let enabled = sc.map(|s| s.enabled).unwrap_or(false);
            let schedule = sc
                .and_then(|s| s.schedule.clone())
                .unwrap_or_else(|| "daily".to_string());

            let source = self.storage.get_source(n)?;
            let recent = match &source {
                Some(src) => self.storage.recent_runs(Some(src.id), 50)?,
                None => Vec::new(),
            };
            let last_started = recent
                .first()
                .and_then(|r| r.get("started_at"))
                .and_then(|v| v.as_str())
                .and_then(|s| parse_iso(Some(s)));
            let next_due = next_due_at(last_started, &schedule);
            let due_now = enabled && is_due(last_started, &schedule, now);

            let Some(src) = source else {
                out.push(SourceStatus {
                    name: n.clone(),
                    type_,
                    enabled,
                    total_items: 0,
                    live_items: 0,
                    deleted_items: 0,
                    last_run_status: None,
                    last_run_at: None,
                    last_mode: None,
                    run_count: 0,
                    watermark: None,
                    has_interrupted_runs: false,
                    schedule,
                    next_due_at: next_due,
                    due_now,
                });
                continue;
            };
            let (total, live, deleted) = self.storage.item_counts(src.id)?;
            let last = recent.first();
            let (_, watermark) = self.storage.load_cursor(src.id)?;
            out.push(SourceStatus {
                name: n.clone(),
                type_,
                enabled,
                total_items: total,
                live_items: live,
                deleted_items: deleted,
                last_run_status: last
                    .and_then(|r| r.get("status"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                last_run_at: last_started,
                last_mode: last
                    .and_then(|r| r.get("mode"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                run_count: self.storage.get_run_count(src.id)?,
                watermark,
                has_interrupted_runs: recent
                    .iter()
                    .any(|r| r.get("status").and_then(|v| v.as_str()) == Some("interrupted")),
                schedule,
                next_due_at: next_due,
                due_now,
            });
        }
        Ok(out)
    }

    /// Recent runs for one source, or across all sources if `name` is
    /// `None`. Empty (not an error) if `name` doesn't match a source
    /// that's ever been backed up.
    pub fn history(&self, name: Option<&str>, limit: u32) -> Result<Vec<ItemRow>, DbsError> {
        let source_id = match name {
            Some(n) => match self.storage.get_source(n)? {
                Some(s) => Some(s.id),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        self.storage.recent_runs(source_id, limit)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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

    // -- choose_mode / vpn_guard_skip / due-date pure-function tests ------

    fn caps(supports_incremental: bool, supports_full_enumeration: bool) -> Capabilities {
        Capabilities {
            supports_incremental,
            supports_full_enumeration,
            requires_auth: false,
            ..Capabilities::default()
        }
    }

    #[test]
    fn choose_mode_force_full_always_wins() {
        let c = caps(true, true);
        assert_eq!(choose_mode("auto", true, false, None, 0, None, &c), "full");
    }

    #[test]
    fn choose_mode_incremental_incapable_connector_always_runs_full() {
        let c = caps(false, true);
        assert_eq!(choose_mode("auto", false, false, None, 5, None, &c), "full");
    }

    #[test]
    fn choose_mode_force_reconcile_requires_full_enumeration_support() {
        let cursor = Cursor {
            value: serde_json::json!({}),
        };
        let with_enum = caps(true, true);
        assert_eq!(
            choose_mode("auto", false, true, Some(&cursor), 1, None, &with_enum),
            "reconcile"
        );
        let without_enum = caps(true, false);
        assert_eq!(
            choose_mode("auto", false, true, Some(&cursor), 1, None, &without_enum),
            "incremental"
        );
    }

    #[test]
    fn choose_mode_first_run_is_full_or_incremental_by_enumeration_support() {
        let with_enum = caps(true, true);
        assert_eq!(
            choose_mode("auto", false, false, None, 0, None, &with_enum),
            "full"
        );
        let without_enum = caps(true, false);
        assert_eq!(
            choose_mode("auto", false, false, None, 0, None, &without_enum),
            "incremental"
        );
    }

    #[test]
    fn choose_mode_explicit_mode_is_honored_with_reconcile_downgrade() {
        let cursor = Cursor {
            value: serde_json::json!({}),
        };
        let c = caps(true, false);
        assert_eq!(
            choose_mode("full", false, false, Some(&cursor), 1, None, &c),
            "full"
        );
        assert_eq!(
            choose_mode("reconcile", false, false, Some(&cursor), 1, None, &c),
            "incremental"
        );
    }

    #[test]
    fn choose_mode_auto_reconciles_every_n_runs() {
        let cursor = Cursor {
            value: serde_json::json!({}),
        };
        let c = caps(true, true);
        assert_eq!(
            choose_mode("auto", false, false, Some(&cursor), 3, Some(3), &c),
            "reconcile"
        );
        assert_eq!(
            choose_mode("auto", false, false, Some(&cursor), 4, Some(3), &c),
            "incremental"
        );
        // 0 is treated as "unset" (falls back to DEFAULT_RECONCILE_EVERY),
        // same as the reference's `sc.reconcile_every_runs or DEFAULT`.
        assert_eq!(
            choose_mode("auto", false, false, Some(&cursor), 7, Some(0), &c),
            "reconcile"
        );
    }

    fn test_source_config(name: &str, type_: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            type_: type_.to_string(),
            enabled: true,
            schedule: None,
            reconcile_every_runs: None,
            store_media: false,
            max_media_mb: 0,
            requires_vpn: false,
            keep_revisions: 0,
            export: None,
            options: HashMap::new(),
        }
    }

    fn test_config(sources: HashMap<String, SourceConfig>) -> Config {
        Config {
            database: ":memory:".to_string(),
            export_dir: String::new(),
            download_root: String::new(),
            default_overlap_seconds: 0,
            vpn_exec: "sudo vpn-netns exec".to_string(),
            vpn_status: String::new(),
            vpn_netns: "rusty-dbs-test-netns-that-does-not-exist".to_string(),
            vpn_guard: VpnGuard::Skip,
            notify_url: None,
            notify_on: crate::config::NotifyOn::default(),
            http_timeout: 30.0,
            http_rate_limit_per_min: 0,
            batch_max: 0,
            sweep_safety_fraction: 0.5,
            parallel: 1,
            sources,
            connectors: HashMap::new(),
            base_dir: std::path::PathBuf::new(),
            source_path: None,
        }
    }

    #[test]
    fn vpn_guard_skip_is_none_when_not_required() {
        let config = test_config(HashMap::new());
        let sc = test_source_config("a", "raindrop");
        assert!(vpn_guard_skip(&config, "a", &sc, Utc::now()).is_none());
    }

    #[test]
    fn vpn_guard_skip_is_none_when_guard_is_off() {
        let mut config = test_config(HashMap::new());
        config.vpn_guard = VpnGuard::Off;
        let mut sc = test_source_config("a", "raindrop");
        sc.requires_vpn = true;
        assert!(vpn_guard_skip(&config, "a", &sc, Utc::now()).is_none());
    }

    #[test]
    fn vpn_guard_skip_skips_when_required_and_not_in_namespace() {
        let config = test_config(HashMap::new());
        let mut sc = test_source_config("a", "raindrop");
        sc.requires_vpn = true;
        let result = vpn_guard_skip(&config, "a", &sc, Utc::now()).unwrap();
        assert_eq!(result.status, RunStatus::Skipped);
        assert!(result.error.unwrap().contains("requires_vpn"));
    }

    #[test]
    fn vpn_guard_skip_proceeds_when_guard_is_warn() {
        let mut config = test_config(HashMap::new());
        config.vpn_guard = VpnGuard::Warn;
        let mut sc = test_source_config("a", "raindrop");
        sc.requires_vpn = true;
        assert!(vpn_guard_skip(&config, "a", &sc, Utc::now()).is_none());
    }

    #[test]
    fn next_due_at_is_none_for_a_never_run_source() {
        assert!(next_due_at(None, "daily").is_none());
    }

    #[test]
    fn is_due_is_true_when_never_run() {
        assert!(is_due(None, "daily", Utc::now()));
    }

    #[test]
    fn is_due_respects_the_daily_slack_window() {
        let now = Utc::now();
        let just_ran = now - ChronoDuration::hours(1);
        assert!(!is_due(Some(just_ran), "daily", now));
        let long_ago = now - ChronoDuration::hours(21);
        assert!(is_due(Some(long_ago), "daily", now));
    }

    // -- BackupService integration tests, against a fuller in-memory double --

    #[derive(Debug, Clone, Default)]
    struct FakeRun {
        source_id: i64,
        status: String,
        mode: String,
        started_at: String,
    }

    #[derive(Default)]
    struct FakeStorage {
        sources: HashMap<String, crate::storage::SourceRecord>,
        next_source_id: i64,
        runs: HashMap<i64, FakeRun>,
        next_run_id: i64,
        run_counts: HashMap<i64, u64>,
        cursors: HashMap<i64, (Option<Cursor>, Option<DateTime<Utc>>)>,
        locks: std::collections::HashSet<i64>,
        counts_by_source: HashMap<i64, (u64, u64, u64)>,
        reap_calls: usize,
    }

    impl Storage for FakeStorage {
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
        ) -> Result<crate::storage::SourceRecord, DbsError> {
            self.next_source_id += 1;
            let id = self
                .sources
                .get(name)
                .map(|s| s.id)
                .unwrap_or(self.next_source_id);
            let record = crate::storage::SourceRecord {
                id,
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
        fn get_source(&self, name: &str) -> Result<Option<crate::storage::SourceRecord>, DbsError> {
            Ok(self.sources.get(name).cloned())
        }
        fn list_sources(&self) -> Result<Vec<crate::storage::SourceRecord>, DbsError> {
            Ok(self.sources.values().cloned().collect())
        }
        fn delete_source(&mut self, _: &str) -> Result<bool, DbsError> {
            unimplemented!()
        }

        fn begin_run(
            &mut self,
            source_id: i64,
            _plugin_id: &str,
            mode: &str,
            _cursor_before: Option<&str>,
        ) -> Result<i64, DbsError> {
            self.next_run_id += 1;
            let run_id = self.next_run_id;
            self.runs.insert(
                run_id,
                FakeRun {
                    source_id,
                    status: "running".to_string(),
                    mode: mode.to_string(),
                    started_at: format!("2026-01-01T00:{:02}:00Z", run_id % 60),
                },
            );
            Ok(run_id)
        }
        fn finish_run(
            &mut self,
            run_id: i64,
            status: &str,
            _stats: &BatchResult,
            _items_seen: u64,
            _cursor_after: Option<&str>,
            _error: Option<&str>,
            _warnings: &[String],
        ) -> Result<(), DbsError> {
            if let Some(run) = self.runs.get_mut(&run_id) {
                run.status = status.to_string();
            }
            Ok(())
        }
        fn reap_interrupted_runs(&mut self) -> Result<Vec<i64>, DbsError> {
            self.reap_calls += 1;
            let ids: Vec<i64> = self
                .runs
                .iter()
                .filter(|(_, r)| r.status == "running")
                .map(|(id, _)| *id)
                .collect();
            for id in &ids {
                self.runs.get_mut(id).unwrap().status = "interrupted".to_string();
            }
            Ok(ids)
        }
        fn recent_runs(
            &self,
            source_id: Option<i64>,
            limit: u32,
        ) -> Result<Vec<ItemRow>, DbsError> {
            let mut rows: Vec<(&i64, &FakeRun)> = self
                .runs
                .iter()
                .filter(|(_, r)| source_id.is_none_or(|sid| r.source_id == sid))
                .collect();
            rows.sort_by(|a, b| b.0.cmp(a.0));
            Ok(rows
                .into_iter()
                .take(limit as usize)
                .map(|(_, r)| {
                    let mut row = ItemRow::new();
                    row.insert(
                        "status".to_string(),
                        serde_json::Value::from(r.status.clone()),
                    );
                    row.insert("mode".to_string(), serde_json::Value::from(r.mode.clone()));
                    row.insert(
                        "started_at".to_string(),
                        serde_json::Value::from(r.started_at.clone()),
                    );
                    row
                })
                .collect())
        }

        fn upsert_items(
            &mut self,
            _: i64,
            _: i64,
            _: &[crate::storage::PreparedItem],
            _: bool,
            _: u64,
        ) -> Result<BatchResult, DbsError> {
            unimplemented!()
        }
        fn soft_delete_missing(
            &mut self,
            _: i64,
            _: &std::collections::HashSet<String>,
            _: i64,
            _: Option<&str>,
        ) -> Result<u64, DbsError> {
            unimplemented!()
        }
        fn live_external_ids(
            &self,
            _: i64,
            _: Option<&str>,
        ) -> Result<std::collections::HashSet<String>, DbsError> {
            unimplemented!()
        }

        fn save_cursor(
            &mut self,
            source_id: i64,
            cursor: Option<&Cursor>,
            watermark: Option<&str>,
            _run_id: i64,
        ) -> Result<(), DbsError> {
            self.cursors
                .insert(source_id, (cursor.cloned(), parse_iso(watermark)));
            Ok(())
        }
        fn load_cursor(
            &self,
            source_id: i64,
        ) -> Result<(Option<Cursor>, Option<DateTime<Utc>>), DbsError> {
            Ok(self
                .cursors
                .get(&source_id)
                .cloned()
                .unwrap_or((None, None)))
        }
        fn get_run_count(&self, source_id: i64) -> Result<u64, DbsError> {
            Ok(*self.run_counts.get(&source_id).unwrap_or(&0))
        }
        fn increment_run_count(&mut self, source_id: i64) -> Result<(), DbsError> {
            *self.run_counts.entry(source_id).or_insert(0) += 1;
            Ok(())
        }

        fn acquire_lock(&mut self, source_id: i64, _run_id: i64) -> Result<bool, DbsError> {
            Ok(self.locks.insert(source_id))
        }
        fn release_lock(&mut self, source_id: i64) -> Result<(), DbsError> {
            self.locks.remove(&source_id);
            Ok(())
        }

        fn iter_items<'a>(
            &'a self,
            _: &crate::storage::ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            unimplemented!()
        }
        fn iter_revisions<'a>(
            &'a self,
            _: &crate::storage::ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            unimplemented!()
        }
        fn item_counts(&self, source_id: i64) -> Result<(u64, u64, u64), DbsError> {
            Ok(*self.counts_by_source.get(&source_id).unwrap_or(&(0, 0, 0)))
        }
        fn browse_items(
            &self,
            _: &crate::storage::ExportQuery,
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

    fn fake_connector(type_name: &str, incremental: bool, full_enum: bool) -> RegisteredConnector {
        RegisteredConnector {
            type_: type_name.to_string(),
            plugin_id: format!("test:{type_name}"),
            dist_name: "test".to_string(),
            is_builtin: true,
            handshake: crate::registry::Handshake {
                type_: type_name.to_string(),
                core_api_version: crate::versioning::CURRENT_API_VERSION,
                schema_version: 1,
                capabilities: caps(incremental, full_enum),
                secret_keys: Vec::new(),
                item_kinds: vec!["item".to_string()],
                display_name: None,
                description: None,
                export_profile: None,
            },
            command: std::path::PathBuf::from("dbs-connector-test"),
            args: Vec::new(),
        }
    }

    struct ScriptedRunner {
        result: std::cell::RefCell<Result<ConnectorRunOutcome, String>>,
    }

    impl ScriptedRunner {
        fn success(stats: BatchResult, items_seen: u64) -> Self {
            Self {
                result: std::cell::RefCell::new(Ok(ConnectorRunOutcome {
                    status: RunStatus::Success,
                    stats,
                    items_seen,
                    cursor_after: None,
                    error: None,
                    warnings: Vec::new(),
                })),
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                result: std::cell::RefCell::new(Err(msg.to_string())),
            }
        }
    }

    impl ConnectorRunner for ScriptedRunner {
        fn run_connector(
            &self,
            _connector: &RegisteredConnector,
            _run_id: i64,
            _source_id: i64,
            _mode: &str,
            _cursor: Option<&Cursor>,
            _since: Option<DateTime<Utc>>,
        ) -> Result<ConnectorRunOutcome, DbsError> {
            match &*self.result.borrow() {
                Ok(outcome) => Ok(outcome.clone()),
                Err(msg) => Err(DbsError::Connector(ConnectorError::Transient(msg.clone()))),
            }
        }
    }

    #[test]
    fn backup_source_errors_for_an_unknown_source() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let err = service
            .backup_source("missing", &BackupSourceOptions::default())
            .unwrap_err();
        assert!(matches!(
            err,
            DbsError::Run(BackupRunError::UnknownSource(_))
        ));
    }

    #[test]
    fn backup_source_skips_a_disabled_source() {
        let mut sc = test_source_config("a", "raindrop");
        sc.enabled = false;
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), sc);
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let result = service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap();
        assert_eq!(result.status, RunStatus::Skipped);
        assert_eq!(result.error.as_deref(), Some("source disabled"));
    }

    #[test]
    fn backup_source_skips_a_vpn_guarded_source() {
        let mut sc = test_source_config("a", "raindrop");
        sc.requires_vpn = true;
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), sc);
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let result = service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap();
        assert_eq!(result.status, RunStatus::Skipped);
        assert!(result.error.unwrap().contains("requires_vpn"));
    }

    #[test]
    fn backup_source_errors_when_connector_type_is_unregistered() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let err = service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap_err();
        assert!(matches!(
            err,
            DbsError::Load(crate::errors::ConnectorLoadError::NotFound(_))
        ));
    }

    #[test]
    fn backup_source_dry_run_never_begins_a_real_run() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let opts = BackupSourceOptions {
            dry_run: true,
            ..BackupSourceOptions::default()
        };
        let result = service.backup_source("a", &opts).unwrap();
        assert_eq!(result.status, RunStatus::Skipped);
        assert_eq!(result.error.as_deref(), Some("dry-run"));
        assert_eq!(result.mode, "full"); // first-ever run, full enumeration supported
    }

    #[test]
    fn backup_source_happy_path_registers_source_and_finishes_the_run() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let stats = BatchResult {
            created: 3,
            ..Default::default()
        };
        let runner = ScriptedRunner::success(stats, 3);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let result = service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap();
        assert_eq!(result.status, RunStatus::Success);
        assert_eq!(result.created, 3);
        assert_eq!(result.fetched, 3);
        assert!(result.run_id.is_some());

        // The source was actually registered and the run recorded.
        assert!(service.storage.get_source("a").unwrap().is_some());
        let history = service.history(Some("a"), 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["status"], serde_json::Value::from("success"));
    }

    #[test]
    fn backup_source_turns_a_runner_error_into_a_failed_result_and_still_finishes_the_run() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::failing("upstream is down");
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let result = service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap();
        assert_eq!(result.status, RunStatus::Failed);
        assert!(result.error.unwrap().contains("upstream is down"));

        let history = service.history(Some("a"), 10).unwrap();
        assert_eq!(history[0]["status"], serde_json::Value::from("failed"));
    }

    #[test]
    fn backup_source_reports_source_locked_when_already_locked() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        // First call registers the source and releases its lock at the end.
        service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap();
        let source_id = service.storage.get_source("a").unwrap().unwrap().id;
        // Simulate a concurrent holder.
        assert!(service.storage.acquire_lock(source_id, 999).unwrap());
        let err = service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap_err();
        assert!(matches!(
            err,
            DbsError::Run(BackupRunError::SourceLocked(_))
        ));
    }

    #[test]
    fn backup_all_skips_disabled_sources_and_batches_the_rest() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));
        let mut disabled = test_source_config("c", "raindrop");
        disabled.enabled = false;
        sources.insert("c".to_string(), disabled);

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let results = service.backup_all(&BackupAllOptions::default()).unwrap();
        assert_eq!(results.len(), 2);
        let mut names: Vec<&str> = results.iter().map(|r| r.source.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
        assert!(results.iter().all(|r| r.status == RunStatus::Success));
    }

    #[test]
    fn backup_all_continue_on_error_isolates_one_sources_failure() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        // "b" has no registered connector, so backup_source errors for it.
        sources.insert("b".to_string(), test_source_config("b", "unregistered"));

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let results = service.backup_all(&BackupAllOptions::default()).unwrap();
        assert_eq!(results.len(), 2);
        let b_result = results.iter().find(|r| r.source == "b").unwrap();
        assert_eq!(b_result.status, RunStatus::Failed);
        let a_result = results.iter().find(|r| r.source == "a").unwrap();
        assert_eq!(a_result.status, RunStatus::Success);
    }

    #[test]
    fn backup_all_without_continue_on_error_aborts_on_first_failure() {
        let mut sources = HashMap::new();
        sources.insert("b".to_string(), test_source_config("b", "unregistered"));
        sources.insert("c".to_string(), test_source_config("c", "raindrop"));

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            continue_on_error: false,
            ..BackupAllOptions::default()
        };
        // "b" sorts before "c", so it fails first and aborts the batch.
        let err = service.backup_all(&opts).unwrap_err();
        assert!(matches!(
            err,
            DbsError::Load(crate::errors::ConnectorLoadError::NotFound(_))
        ));
    }

    #[test]
    fn status_reports_zeroed_snapshot_for_a_never_backed_up_source() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let statuses = service.status(Some("a")).unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].name, "a");
        assert_eq!(statuses[0].total_items, 0);
        assert!(statuses[0].last_run_status.is_none());
    }

    #[test]
    fn status_reflects_item_counts_and_last_run_after_a_backup() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        // FakeStorage::upsert_source assigns the first-ever source id 1
        // deterministically, so this can be pre-seeded before the backup
        // run that will register "a" as that first source.
        storage.counts_by_source.insert(1, (5, 5, 0));
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let stats = BatchResult {
            created: 5,
            ..Default::default()
        };
        let runner = ScriptedRunner::success(stats, 5);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        service
            .backup_source("a", &BackupSourceOptions::default())
            .unwrap();

        let statuses = service.status(None).unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].total_items, 5);
        assert_eq!(statuses[0].last_run_status.as_deref(), Some("success"));
        assert_eq!(statuses[0].last_mode.as_deref(), Some("full"));
    }

    #[test]
    fn history_returns_empty_for_an_unknown_source_name() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        assert!(service.history(Some("missing"), 10).unwrap().is_empty());
    }
}
