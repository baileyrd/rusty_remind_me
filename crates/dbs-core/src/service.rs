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

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::Value;

use crate::cancel::CancelToken;
use crate::capabilities::{AuthCapture, Capabilities, ItemKind};
use crate::config::{Config, SourceConfig, VpnGuard};
use crate::crypto::{
    decrypt_file, is_encrypted, resolve_passphrase, EncryptingWriter, DEFAULT_PASSPHRASE_ENV,
};
use crate::errors::{BackupRunError, ConnectorError, ConnectorLoadError, DbsError};
use crate::export::{get_exporter, ExportResult, ExportSource};
use crate::export_profile::{resolve_export_profile, ExportProfile};
use crate::models::{
    ConnectorInfo, Cursor, DoctorCheck, ProgressEvent, ProgressPhase, RestoreReport, RunResult,
    RunStatus, SourceStatus, VerifyIssue, VerifyReport,
};
use crate::netns::{in_named_netns, named_netns_exists};
use crate::registry::{ConnectorRegistry, RegisteredConnector};
use crate::restore::{
    iter_export_rows, prepared_item_from_row, read_manifest, skipped_extras, verify_archive,
};
use crate::storage::migrations::SCHEMA_VERSION;
use crate::storage::{BatchResult, ExportQuery, ItemRow, PreparedItem, Storage};
use crate::timeutil::{iso_z, parse_iso};

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

/// Serializes a JSON value back to a TOML literal, for
/// [`BackupService::add_source`] appending `--set key=value` pairs to
/// the config file. Mirrors the reference's `_toml_value`.
fn toml_value(value: &Value) -> String {
    match value {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Null => "\"\"".to_string(),
        other => {
            let text = other
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| other.to_string());
            let escaped = text
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            format!("\"{escaped}\"")
        }
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
///
/// `Send + Sync` so a single runner can be shared read-only across the
/// worker threads `backup --all --parallel N` spawns.
pub trait ConnectorRunner: Send + Sync {
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

/// Receiver for [`ProgressEvent`]s emitted during a backup run — the
/// seam a CLI live-progress line (or a future web tier) renders over.
/// Mirrors the reference's `ProgressCallback = Callable[[ProgressEvent],
/// None]`; a trait (rather than a bare `Fn` alias) so a stateful
/// renderer can implement it directly instead of closing over interior
/// mutability. `Sync` so one sink can be shared read-only across
/// `backup --all --parallel N` worker threads.
///
/// **Scope note (issue #67):** `BackupService::backup_source` currently
/// hands off to [`ConnectorRunner`] as a single blocking call — there's
/// no run/stream protocol yet (ADR-0001 steps 2-3) to report
/// per-item progress from. Only [`ProgressPhase::SourceStart`] and
/// [`ProgressPhase::SourceDone`] are emitted today; `Item`/
/// `Checkpoint`/`Sweep` stay reserved on the enum for that follow-up
/// issue to start emitting through this same seam without an API
/// break.
pub trait ProgressSink: Sync {
    fn emit(&self, event: &ProgressEvent);
}

impl<F: Fn(&ProgressEvent) + Sync> ProgressSink for F {
    fn emit(&self, event: &ProgressEvent) {
        self(event)
    }
}

/// Wraps an inner [`ProgressSink`] to fill in `source_index`/
/// `source_total` for `dbs backup --all` — mirrors the reference's
/// `_frame_progress`.
struct FramedProgress<'a> {
    inner: &'a dyn ProgressSink,
    index: u32,
    total: u32,
}

impl ProgressSink for FramedProgress<'_> {
    fn emit(&self, event: &ProgressEvent) {
        let mut framed = event.clone();
        framed.source_index = Some(self.index);
        framed.source_total = Some(self.total);
        self.inner.emit(&framed);
    }
}

/// Wraps an inner [`ProgressSink`] with a lock so events from several
/// `--parallel` worker threads never interleave — mirrors the
/// reference's `threading.Lock`-guarded `safe_progress`.
struct LockedProgress<'a> {
    inner: &'a dyn ProgressSink,
    lock: &'a Mutex<()>,
}

impl ProgressSink for LockedProgress<'_> {
    fn emit(&self, event: &ProgressEvent) {
        let _guard = self.lock.lock().unwrap();
        self.inner.emit(event);
    }
}

/// Options for [`BackupService::backup_source`]. `mode` is one of
/// `"auto"` (default), `"incremental"`, `"reconcile"`, or `"full"`.
#[derive(Clone)]
pub struct BackupSourceOptions<'a> {
    pub mode: String,
    pub force_full: bool,
    pub force_reconcile: bool,
    pub dry_run: bool,
    pub limit: Option<u32>,
    /// Whether this call should itself reap interrupted runs — `false`
    /// when called from `backup_all`, which reaps once up front instead.
    pub reap: bool,
    /// Receives [`ProgressPhase::SourceStart`]/[`ProgressPhase::SourceDone`]
    /// for this source's run — see [`ProgressSink`] for what's emitted
    /// today and why.
    pub on_progress: Option<&'a dyn ProgressSink>,
}

impl<'a> Default for BackupSourceOptions<'a> {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            force_full: false,
            force_reconcile: false,
            dry_run: false,
            limit: None,
            reap: true,
            on_progress: None,
        }
    }
}

/// Options for [`BackupService::backup_all`], run in name-sorted order
/// (the reference preserves TOML declaration order via a Python dict;
/// this crate's `Config::sources` is a `HashMap`, so sorted-by-name is
/// the deterministic substitute).
#[derive(Clone)]
pub struct BackupAllOptions<'a> {
    /// Skip a source whose `schedule` cadence hasn't elapsed since its
    /// last run (see [`crate::service::BackupService`]'s private
    /// `is_due`/`next_due_at`, ported from the reference's
    /// `_is_due`/`_next_due_at`).
    pub only_due: bool,
    pub continue_on_error: bool,
    pub force_full: bool,
    pub force_reconcile: bool,
    pub dry_run: bool,
    pub limit: Option<u32>,
    /// Worker-pool size for concurrent source backups. `None` falls
    /// back to `Config::parallel`. A resolved value of `1` (or a
    /// single-source work-list, or `dry_run`) runs the plain
    /// sequential path — mirrors the reference's `parallel` param on
    /// `backup_all`.
    pub parallel: Option<u32>,
    /// Receives each source's [`ProgressEvent`]s, framed with
    /// `source_index`/`source_total` (see [`FramedProgress`]) and,
    /// on the parallel path, serialized so concurrent workers never
    /// interleave a delivery (see [`LockedProgress`]).
    pub on_progress: Option<&'a dyn ProgressSink>,
    /// Checked between sources: a cancelled token stops the batch
    /// from starting any source not already in flight (in-flight
    /// sources still finish and commit) — mirrors the reference's
    /// `backup_all(cancel=...)`. Ctrl+C in the CLI sets this.
    pub cancel: Option<CancelToken>,
}

impl<'a> Default for BackupAllOptions<'a> {
    fn default() -> Self {
        Self {
            only_due: false,
            continue_on_error: true,
            force_full: false,
            force_reconcile: false,
            dry_run: false,
            limit: None,
            parallel: None,
            on_progress: None,
            cancel: None,
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
        opts: &BackupSourceOptions<'_>,
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

        if let Some(sink) = opts.on_progress {
            sink.emit(&ProgressEvent {
                phase: ProgressPhase::SourceStart,
                source: name.to_string(),
                mode: chosen_mode.clone(),
                fetched: 0,
                created: 0,
                updated: 0,
                unchanged: 0,
                deleted: 0,
                source_index: None,
                source_total: None,
                result: None,
                note: String::new(),
            });
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

        let result = RunResult {
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
        };

        if let Some(sink) = opts.on_progress {
            sink.emit(&ProgressEvent {
                phase: ProgressPhase::SourceDone,
                source: result.source.clone(),
                mode: result.mode.clone(),
                fetched: result.fetched,
                created: result.created,
                updated: result.updated,
                unchanged: result.unchanged,
                deleted: result.deleted,
                source_index: None,
                source_total: None,
                result: Some(result.clone()),
                note: String::new(),
            });
        }

        Ok(result)
    }

    /// Mirrors the reference's `_is_due`: `true` if `name`'s `schedule`
    /// cadence has elapsed since its last run (or it has never run at
    /// all — a never-run source is always due).
    fn source_is_due(&self, name: &str, now: DateTime<Utc>) -> Result<bool, DbsError> {
        let last_started = match self.storage.get_source(name)? {
            Some(src) => self
                .storage
                .recent_runs(Some(src.id), 1)?
                .first()
                .and_then(|r| r.get("started_at"))
                .and_then(|v| v.as_str())
                .and_then(|s| parse_iso(Some(s))),
            None => None,
        };
        let schedule = self
            .config
            .sources
            .get(name)
            .and_then(|sc| sc.schedule.clone())
            .unwrap_or_else(|| "daily".to_string());
        Ok(is_due(last_started, &schedule, now))
    }

    /// Backs up every enabled source, in name-sorted order — sequentially,
    /// or on a bounded worker pool when `--parallel N` resolves above 1
    /// (see [`Self::backup_all_parallel`]). Reaps once, up front, while no
    /// run of ours is live yet — a per-source reap mid-batch would flip a
    /// sibling's genuinely-running row.
    ///
    /// `opts.cancel`, if set, is checked before each source starts (the
    /// sequential path) or before each dequeue (the parallel path): a
    /// source already in flight always finishes and commits, but no new
    /// one starts once cancelled — the CLI's Ctrl+C handling (#67) sets
    /// this. `opts.on_progress` receives `SourceStart`/`SourceDone` for
    /// each source, framed with `source_index`/`source_total`.
    pub fn backup_all(&mut self, opts: &BackupAllOptions<'_>) -> Result<Vec<RunResult>, DbsError> {
        self.storage.reap_interrupted_runs()?;

        let mut names: Vec<String> = self
            .config
            .sources
            .iter()
            .filter(|(_, sc)| sc.enabled)
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();

        if opts.only_due {
            let now = Utc::now();
            let mut due = Vec::with_capacity(names.len());
            for name in names {
                if self.source_is_due(&name, now)? {
                    due.push(name);
                }
            }
            names = due;
        }

        let per_source = BackupSourceOptions {
            mode: "auto".to_string(),
            force_full: opts.force_full,
            force_reconcile: opts.force_reconcile,
            dry_run: opts.dry_run,
            limit: opts.limit,
            reap: false,
            on_progress: None,
        };

        let requested_workers = opts.parallel.unwrap_or(self.config.parallel).max(1);
        // A dry-run only resolves each source's chosen mode — no connector
        // runs, so there is nothing to parallelize; keep it on the simple
        // sequential path (which threads dry_run through to backup_source).
        if requested_workers > 1 && names.len() > 1 && !opts.dry_run {
            let workers = (requested_workers as usize).min(names.len());
            if let Some(outcome) = self.backup_all_parallel(
                &names,
                workers,
                &per_source,
                opts.continue_on_error,
                opts.on_progress,
                opts.cancel.clone(),
            ) {
                return outcome;
            }
            // Storage can't provide worker connections (e.g. an in-memory
            // database) — fall through to the sequential path below.
        }

        let total = names.len() as u32;
        let mut results = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            if opts.cancel.as_ref().is_some_and(CancelToken::cancelled) {
                break;
            }
            let framed = opts.on_progress.map(|sink| FramedProgress {
                inner: sink,
                index: i as u32 + 1,
                total,
            });
            let mut source_opts = per_source.clone();
            source_opts.on_progress = framed.as_ref().map(|f| f as &dyn ProgressSink);
            match self.backup_source(name, &source_opts) {
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

    /// Runs `names` on a bounded thread pool (`--parallel N`), one
    /// [`BackupSourceOptions`] shared across all of them. Returns `None`
    /// when the storage backend can't provide `workers` independent
    /// connections (e.g. an in-memory database) — the caller falls back
    /// to the sequential path.
    ///
    /// **Sync-threadpool decision (issue #66):** this crate already
    /// chose `reqwest::blocking` over `tokio` for the HTTP client
    /// (#22); a worker pool built on plain `std::thread::scope` keeps
    /// that same synchronous model rather than pulling in an async
    /// runtime (or `rayon`) for this one feature. Each worker thread
    /// gets its own [`Storage::spawn`] connection — SQLite's WAL mode
    /// plus `busy_timeout` arbitrate the single writer slot, and the
    /// existing per-source lock table (`acquire_lock`) still prevents
    /// double-running a source — so nothing but the read-only
    /// `Config`/`ConnectorRegistry`/`ConnectorRunner` references cross
    /// a thread boundary; `self.storage` (this call's own connection)
    /// is never touched by a worker. Work is pulled from a shared
    /// queue (rather than statically chunked) so a fast source doesn't
    /// leave its worker idle while a slow one is still running
    /// elsewhere. Per-source failures are isolated exactly as in the
    /// sequential path: `continue_on_error` turns them into a `Failed`
    /// [`RunResult`] instead of aborting the batch; when it's `false`,
    /// the first error stops new work from being dequeued (sources
    /// already in flight still finish) and is returned as `Err`. A
    /// cancelled `cancel` token has the same "stop dequeuing, let
    /// in-flight work finish" effect. `on_progress`, if set, is framed
    /// per source (`source_index`/`source_total`) and lock-serialized
    /// so concurrent workers' deliveries never interleave.
    #[allow(clippy::too_many_arguments)]
    fn backup_all_parallel(
        &self,
        names: &[String],
        workers: usize,
        per_source: &BackupSourceOptions<'_>,
        continue_on_error: bool,
        on_progress: Option<&dyn ProgressSink>,
        cancel: Option<CancelToken>,
    ) -> Option<Result<Vec<RunResult>, DbsError>> {
        let mut worker_storages: Vec<Box<dyn Storage>> = Vec::with_capacity(workers);
        for _ in 0..workers {
            worker_storages.push(self.storage.spawn()?);
        }

        let config = self.config;
        let registry = self.registry;
        let runner = self.runner;
        let total = names.len() as u32;
        let queue: Mutex<VecDeque<(usize, String)>> =
            Mutex::new(names.iter().cloned().enumerate().collect());
        let results: Mutex<Vec<Option<RunResult>>> = Mutex::new(vec![None; names.len()]);
        let first_error: Mutex<Option<DbsError>> = Mutex::new(None);
        let stop = AtomicBool::new(false);
        let progress_lock: Mutex<()> = Mutex::new(());

        std::thread::scope(|scope| {
            for mut storage in worker_storages {
                let queue = &queue;
                let results = &results;
                let first_error = &first_error;
                let stop = &stop;
                let progress_lock = &progress_lock;
                let cancel = cancel.clone();
                scope.spawn(move || {
                    loop {
                        if stop.load(Ordering::Relaxed)
                            || cancel.as_ref().is_some_and(CancelToken::cancelled)
                        {
                            break;
                        }
                        let next = queue.lock().unwrap().pop_front();
                        let (idx, name) = match next {
                            Some(v) => v,
                            None => break,
                        };
                        let locked = on_progress.map(|sink| LockedProgress {
                            inner: sink,
                            lock: progress_lock,
                        });
                        let framed = locked.as_ref().map(|l| FramedProgress {
                            inner: l as &dyn ProgressSink,
                            index: idx as u32 + 1,
                            total,
                        });
                        let mut source_opts = per_source.clone();
                        source_opts.on_progress = framed.as_ref().map(|f| f as &dyn ProgressSink);

                        let mut svc =
                            BackupService::new(storage.as_mut(), config, registry, runner);
                        match svc.backup_source(&name, &source_opts) {
                            Ok(r) => results.lock().unwrap()[idx] = Some(r),
                            Err(e) => {
                                if continue_on_error {
                                    let now = Utc::now();
                                    results.lock().unwrap()[idx] =
                                        Some(RunResult::failed(&name, now, e.to_string()));
                                } else {
                                    stop.store(true, Ordering::Relaxed);
                                    let mut first_error = first_error.lock().unwrap();
                                    if first_error.is_none() {
                                        *first_error = Some(e);
                                    }
                                }
                            }
                        }
                    }
                    storage.close();
                });
            }
        });

        if let Some(e) = first_error.into_inner().unwrap() {
            return Some(Err(e));
        }
        Some(Ok(results
            .into_inner()
            .unwrap()
            .into_iter()
            .flatten()
            .collect()))
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

    /// Paginated, filterable, searchable item listing — the CLI/web
    /// counterpart of the Browse tab. Delegates to
    /// [`Storage::browse_items`] (FTS5 when the backend has it, LIKE
    /// otherwise — see [`crate::storage::sqlite_storage`]'s doc-comment).
    pub fn browse_items(
        &self,
        query: &ExportQuery,
        text: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<ItemRow>, u64), DbsError> {
        self.storage.browse_items(query, text, limit, offset)
    }

    /// Full detail for one item (raw payload + its media list), by
    /// internal id. Delegates to [`Storage::get_item`].
    pub fn get_item(&self, item_id: i64) -> Result<Option<ItemRow>, DbsError> {
        self.storage.get_item(item_id)
    }

    /// Aggregate item/media/revision counts — the CLI/web counterpart
    /// of the metrics strip. Delegates to [`Storage::metrics`].
    pub fn metrics(&self) -> Result<ItemRow, DbsError> {
        self.storage.metrics()
    }

    /// One row per configured source, in config order — `name`/`type`/
    /// `enabled`/`schedule`/`backed_up` (whether it has ever produced a
    /// `sources` row, i.e. run at least once). Mirrors the reference's
    /// `list_sources`.
    pub fn list_sources(&self) -> Result<Vec<ItemRow>, DbsError> {
        let mut names: Vec<&String> = self.config.sources.keys().collect();
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let sc = &self.config.sources[name];
            let backed_up = self.storage.get_source(name)?.is_some();
            let mut row = ItemRow::new();
            row.insert("name".to_string(), Value::from(name.clone()));
            row.insert("type".to_string(), Value::from(sc.type_.clone()));
            row.insert("enabled".to_string(), Value::from(sc.enabled));
            row.insert(
                "schedule".to_string(),
                sc.schedule.clone().map(Value::from).unwrap_or(Value::Null),
            );
            row.insert("backed_up".to_string(), Value::from(backed_up));
            out.push(row);
        }
        Ok(out)
    }

    /// One [`ConnectorInfo`] per registry entry, in discovery order.
    /// Mirrors the reference's `list_connectors`. `item_kinds` carries
    /// only the handshake's bare kind names — the connector's own
    /// richer per-kind `display_name`/`description` (Python's
    /// `cls.item_kinds`) isn't part of the spawn/handshake protocol
    /// (ADR-0001 step 1), so both fields fall back to the name itself.
    pub fn list_connectors(&self) -> Vec<ConnectorInfo> {
        self.registry
            .all()
            .into_iter()
            .map(|rc| ConnectorInfo {
                type_: rc.type_.clone(),
                plugin_id: rc.plugin_id.clone(),
                dist_name: rc.dist_name.clone(),
                is_builtin: rc.is_builtin,
                display_name: rc
                    .handshake
                    .display_name
                    .clone()
                    .unwrap_or_else(|| rc.type_.clone()),
                description: rc.handshake.description.clone().unwrap_or_default(),
                capabilities: rc.handshake.capabilities.clone(),
                item_kinds: rc
                    .handshake
                    .item_kinds
                    .iter()
                    .map(|k| ItemKind {
                        name: k.clone(),
                        display_name: k.clone(),
                        description: String::new(),
                    })
                    .collect(),
                secret_keys: rc.handshake.secret_keys.clone(),
                config_schema: Value::Null,
            })
            .collect()
    }

    /// Validates every configured source's connector type is
    /// resolvable in the registry. Returns `(name, error_or_none)`,
    /// in config order. Mirrors the reference's `check_sources` — the
    /// per-option Pydantic-model validation it also does has no
    /// analogue here (a subprocess connector doesn't expose a config
    /// schema to validate against in-process, only at handshake time),
    /// so this checks connector *resolvability*, the one thing this
    /// registry can answer without spawning anything.
    pub fn check_sources(&self) -> Vec<(String, Option<String>)> {
        let mut names: Vec<&String> = self.config.sources.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let sc = &self.config.sources[name];
                let err = if self.registry.get(&sc.type_).is_some() {
                    None
                } else {
                    Some(format!("connector plugin not found: {}", sc.type_))
                };
                (name.clone(), err)
            })
            .collect()
    }

    /// Resolves a `dbs capture TARGET` argument: first as a connector
    /// type directly, then (if that doesn't resolve) as a configured
    /// source name whose own connector type is looked up. Mirrors the
    /// reference's target resolution in the `capture` command. Errors
    /// if neither resolves, or if the resolved connector declares no
    /// [`AuthCapture`] (nothing to interactively capture — e.g. a
    /// connector authenticated purely by an API token).
    pub fn resolve_capture_target(
        &self,
        target: &str,
    ) -> Result<(RegisteredConnector, AuthCapture), DbsError> {
        let rc = match self.registry.get(target) {
            Some(rc) => rc.clone(),
            None => {
                let sc = self.config.sources.get(target).ok_or_else(|| {
                    DbsError::Config(format!("no such connector or source: {target:?}"))
                })?;
                self.registry
                    .get(&sc.type_)
                    .cloned()
                    .ok_or_else(|| DbsError::Load(ConnectorLoadError::NotFound(sc.type_.clone())))?
            }
        };
        let spec = rc.handshake.auth_capture.clone().ok_or_else(|| {
            DbsError::Config(format!("{target:?} has no interactive auth capture"))
        })?;
        Ok((rc, spec))
    }

    /// Selects candidate YouTube videos from the backup database for
    /// `dbs research youtube-backup`, mirroring the reference's
    /// `research/from_backup.py::videos_from_rows`: queries already
    /// backed-up items of kind `"video"` (optionally restricted to
    /// `sources` by name), keeps only rows from `youtube`-type sources
    /// with a parseable id in `raw`, optionally filters by
    /// `raw.list_label` (`lists`, e.g. `watch-later`/`liked`/
    /// `playlist:Music`), collapses the same video saved under
    /// multiple lists to one row (first-seen-wins), then truncates to
    /// `limit`.
    ///
    /// Returns full [`ItemRow`]s rather than a narrower video model —
    /// the NotebookLM synthesis step that would need one doesn't exist
    /// in this port yet (see gap-analysis.md's Research subsystem
    /// row).
    pub fn select_youtube_backup_videos(
        &self,
        sources: Option<&[String]>,
        lists: Option<&[String]>,
        limit: Option<usize>,
    ) -> Result<Vec<ItemRow>, DbsError> {
        let query = ExportQuery {
            sources: sources.map(|s| s.to_vec()),
            item_types: Some(vec!["video".to_string()]),
            ..Default::default()
        };
        let rows = self.storage.iter_items(&query)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for row in rows {
            if row.get("type").and_then(Value::as_str) != Some("youtube") {
                continue;
            }
            let Some(raw) = row.get("raw").and_then(Value::as_object) else {
                continue;
            };
            let vid = raw.get("id").and_then(Value::as_str).unwrap_or("").trim();
            if vid.is_empty() {
                continue;
            }
            if let Some(lists) = lists {
                let list_label = raw.get("list_label").and_then(Value::as_str);
                if !list_label.is_some_and(|l| lists.iter().any(|x| x == l)) {
                    continue;
                }
            }
            if !seen.insert(vid.to_string()) {
                continue;
            }
            out.push(row);
            if let Some(limit) = limit {
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Validates `name`/`type_` (name not already configured, `type_`
    /// resolvable in the registry) and, if `self.config.source_path` is
    /// set, appends a `[sources.NAME]` block to that file. Mirrors the
    /// reference's `add_source` — minus the Pydantic option validation
    /// [`Self::check_sources`]'s doc-comment explains isn't available
    /// here; `options` are written through verbatim.
    ///
    /// Does **not** update `self.config` in memory (unlike the
    /// reference) — `BackupService` only borrows its `Config`
    /// (`&'a Config`, not owned), and the CLI's short-lived process
    /// re-reads the file on its next invocation anyway, so there's
    /// nothing in-process left to keep in sync.
    pub fn add_source(
        &self,
        name: &str,
        type_: &str,
        options: &HashMap<String, Value>,
        store_media: bool,
        max_media_mb: u32,
        requires_vpn: bool,
    ) -> Result<(), DbsError> {
        if self.config.sources.contains_key(name) {
            return Err(DbsError::Config(format!(
                "source {name:?} already exists in config"
            )));
        }
        self.registry
            .get(type_)
            .ok_or_else(|| DbsError::Load(ConnectorLoadError::NotFound(type_.to_string())))?;

        let Some(path) = &self.config.source_path else {
            return Err(DbsError::Config(
                "no config file to append the new source to".to_string(),
            ));
        };
        let mut block = format!("\n[sources.{name}]\ntype = \"{type_}\"\nenabled = true\n");
        if store_media {
            block.push_str("store_media = true\n");
            if max_media_mb > 0 {
                block.push_str(&format!("max_media_mb = {max_media_mb}\n"));
            }
        }
        if requires_vpn {
            block.push_str("requires_vpn = true\n");
        }
        let mut option_keys: Vec<&String> = options.keys().collect();
        option_keys.sort();
        for key in option_keys {
            block.push_str(&format!("{key} = {}\n", toml_value(&options[key])));
        }

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(|e| DbsError::Storage(format!("failed to open config file: {e}")))?;
        file.write_all(block.as_bytes())
            .map_err(|e| DbsError::Storage(format!("failed to append source to config: {e}")))?;
        Ok(())
    }

    /// Environment/health diagnostics — the README's troubleshooting
    /// checklist as a command. Read-only; never mutates anything.
    /// Mirrors the reference's `doctor`, minus two checks that don't
    /// have an equivalent in this port's architecture (documented at
    /// the call site, not silently dropped):
    ///
    /// * `source.NAME.config` (Pydantic option validation) and
    ///   `source.NAME.deps` (Python runtime-dependency importability)
    ///   assume an in-process connector class; this port's connectors
    ///   are external subprocesses whose only interface is the
    ///   spawn/handshake protocol (ADR-0001 step 1), same gap
    ///   [`Self::check_sources`]'s doc-comment explains.
    /// * `deps.yt-dlp` checks a Python package this Rust binary
    ///   doesn't depend on — not applicable until a connector/download
    ///   pipeline issue introduces whatever tooling this port uses for
    ///   that.
    ///
    /// `secret_store`, like [`crate::crypto::resolve_passphrase`]'s
    /// parameter of the same name, is the caller's `.env`/environment
    /// map — `BackupService` doesn't own one itself.
    pub fn doctor(&self, secret_store: Option<&HashMap<String, String>>) -> Vec<DoctorCheck> {
        let mut checks = Vec::new();

        let integrity = self
            .storage
            .integrity_check()
            .unwrap_or_else(|e| e.to_string());
        checks.push(DoctorCheck {
            name: "database.integrity".to_string(),
            status: if integrity == "ok" { "ok" } else { "fail" }.to_string(),
            detail: integrity,
        });

        let wal_path = format!("{}-wal", self.config.database);
        let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        const WAL_WARN_BYTES: u64 = 10_000_000;
        checks.push(DoctorCheck {
            name: "database.wal".to_string(),
            status: if wal_bytes > WAL_WARN_BYTES {
                "warn"
            } else {
                "ok"
            }
            .to_string(),
            detail: if wal_bytes > WAL_WARN_BYTES {
                format!("{wal_bytes} bytes — run `dbs maintain` to fold it into the main file")
            } else {
                format!("{wal_bytes} bytes")
            },
        });

        let interrupted = self
            .storage
            .recent_runs(None, 50)
            .map(|runs| {
                runs.iter()
                    .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("interrupted"))
                    .count()
            })
            .unwrap_or(0);
        checks.push(DoctorCheck {
            name: "runs.interrupted".to_string(),
            status: if interrupted > 0 { "warn" } else { "ok" }.to_string(),
            detail: format!("{interrupted} interrupted run(s) in recent history")
                + if interrupted > 0 {
                    " \u{2014} a crash/kill; the next backup resumes from the last committed cursor"
                } else {
                    ""
                },
        });

        let mut names: Vec<&String> = self.config.sources.keys().collect();
        names.sort();
        for name in names {
            let sc = &self.config.sources[name];
            if !sc.enabled {
                checks.push(DoctorCheck {
                    name: format!("source.{name}"),
                    status: "ok".to_string(),
                    detail: "disabled".to_string(),
                });
                continue;
            }
            let Some(rc) = self.registry.get(&sc.type_) else {
                checks.push(DoctorCheck {
                    name: format!("source.{name}"),
                    status: "fail".to_string(),
                    detail: format!("connector {:?} unavailable: not found", sc.type_),
                });
                continue;
            };

            if sc.requires_vpn {
                checks.push(self.vpn_doctor_check(name));
            }

            let declared = &rc.handshake.secret_keys;
            if rc.handshake.capabilities.requires_auth && !declared.is_empty() {
                let present: Vec<&str> = declared
                    .iter()
                    .filter(|k| {
                        secret_store
                            .and_then(|s| s.get(k.as_str()))
                            .is_some_and(|v| !v.is_empty())
                    })
                    .map(String::as_str)
                    .collect();
                checks.push(DoctorCheck {
                    name: format!("source.{name}.secrets"),
                    status: if present.is_empty() { "fail" } else { "ok" }.to_string(),
                    detail: if present.is_empty() {
                        format!(
                            "none of {} is set \u{2014} the run will fail at auth",
                            declared.join(", ")
                        )
                    } else {
                        format!("set: {}", present.join(", "))
                    },
                });
            }

            if let Some(source) = self.storage.get_source(name).ok().flatten() {
                let last_ok = self
                    .storage
                    .recent_runs(Some(source.id), 50)
                    .ok()
                    .and_then(|runs| {
                        runs.iter()
                            .find(|r| r.get("status").and_then(|v| v.as_str()) == Some("success"))
                            .and_then(|r| r.get("started_at"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| parse_iso(Some(s)))
                    });
                if let Some(last_ok) = last_ok {
                    let schedule = sc.schedule.clone().unwrap_or_else(|| "daily".to_string());
                    let slack = schedule_slack(&schedule);
                    if Utc::now() - last_ok > slack + slack {
                        checks.push(DoctorCheck {
                            name: format!("source.{name}.staleness"),
                            status: "warn".to_string(),
                            detail: format!(
                                "last successful backup was {} \u{2014} more than twice the {schedule} cadence ago",
                                iso_z(last_ok)
                            ),
                        });
                    }
                }
            }
        }

        checks
    }

    /// Readiness of a `requires_vpn` source's VPN routing. Mirrors the
    /// reference's `_vpn_doctor_check` (see [`vpn_guard_skip`] for the
    /// enforcement side of the same check).
    fn vpn_doctor_check(&self, name: &str) -> DoctorCheck {
        let ns = &self.config.vpn_netns;
        let check_name = format!("source.{name}.vpn");
        if self.config.vpn_guard == VpnGuard::Off {
            return DoctorCheck {
                name: check_name,
                status: "ok".to_string(),
                detail: "requires_vpn set but vpn_guard=off (not enforced)".to_string(),
            };
        }
        if in_named_netns(ns) {
            return DoctorCheck {
                name: check_name,
                status: "ok".to_string(),
                detail: format!("running inside the {ns:?} netns"),
            };
        }
        if named_netns_exists(ns) {
            return DoctorCheck {
                name: check_name,
                status: "ok".to_string(),
                detail: format!(
                    "requires VPN; the {ns:?} netns is up \u{2014} run via `{} dbs backup {name}` (a direct run here is skipped)",
                    self.config.vpn_exec
                ),
            };
        }
        DoctorCheck {
            name: check_name,
            status: "warn".to_string(),
            detail: format!(
                "requires VPN but the {ns:?} netns is not up \u{2014} start it (e.g. `sudo systemctl start vpn-netns`), then run via `{}`",
                self.config.vpn_exec
            ),
        }
    }

    /// Replays an export (archive zip or raw-bearing ndjson) into the
    /// DB. Mirrors the reference's `BackupService.restore`.
    ///
    /// Rows go through the same classified [`Storage::upsert_items`]
    /// path a live backup uses, carrying their stored `content_hash`
    /// verbatim, so a re-restore of the same bundle is a no-op
    /// ("unchanged"). Existing sources are never reconfigured — a
    /// source row is created only when missing (type from the bundle,
    /// empty config). Cursors are untouched: a freshly restored source
    /// simply does a full run on its next backup. Each restored source
    /// gets a `mode="restore"` entry in run history.
    ///
    /// An encrypted bundle is decrypted to a private temp file first —
    /// the passphrase comes from `secret_store` or the environment,
    /// never argv (see [`crate::crypto`]).
    pub fn restore(
        &mut self,
        path: &Path,
        dry_run: bool,
        secret_store: Option<&HashMap<String, String>>,
    ) -> Result<RestoreReport, DbsError> {
        if !path.is_file() {
            return Err(DbsError::Config(format!(
                "no such file: {}",
                path.display()
            )));
        }

        if is_encrypted(path) {
            let passphrase = resolve_passphrase(secret_store, DEFAULT_PASSPHRASE_ENV)?;
            let tmp_dir = std::env::temp_dir().join(format!(
                "dbs-restore-{}-{}",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            std::fs::create_dir_all(&tmp_dir)
                .map_err(|e| DbsError::Storage(format!("failed to create temp dir: {e}")))?;
            let plain = tmp_dir.join("bundle");
            let result = decrypt_file(path, &plain, &passphrase).and_then(|_| {
                self.restore(&plain, dry_run, secret_store)
                    .map(|mut report| {
                        report.path = path.display().to_string();
                        report
                    })
            });
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return result;
        }

        let manifest = read_manifest(path)?;
        if manifest.is_some() {
            // A checksummed bundle is verified before a single row is
            // ingested; a corrupt or tampered bundle must never be
            // partially restored.
            let integrity = verify_archive(path)?;
            if !integrity.issues.is_empty() {
                return Err(DbsError::Config(format!(
                    "bundle failed integrity verification: {}",
                    integrity
                        .issues
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )));
            }
        }
        if let Some(bundle_schema) = manifest
            .as_ref()
            .and_then(|m| m.get("db_schema_version"))
            .and_then(|v| v.as_i64())
        {
            if bundle_schema > SCHEMA_VERSION {
                return Err(DbsError::Config(format!(
                    "bundle was written by a newer dbs (db_schema_version {bundle_schema} > this build's {SCHEMA_VERSION}); upgrade dbs before restoring."
                )));
            }
        }

        let mut warnings: Vec<String> = Vec::new();
        let (revisions_skipped, media_skipped) = skipped_extras(manifest.as_ref());
        if revisions_skipped > 0 {
            warnings.push(format!(
                "{revisions_skipped} revision row(s) in the bundle were not restored (restore replays the latest item state only)"
            ));
        }
        if media_skipped > 0 {
            warnings.push(format!(
                "{media_skipped} media file(s) in the bundle were not restored"
            ));
        }

        let rows = iter_export_rows(path)?;

        let mut fetched: u64 = 0;
        let mut seen: HashMap<String, u64> = HashMap::new();
        let mut records: HashMap<String, crate::storage::SourceRecord> = HashMap::new();
        let mut runs: HashMap<String, i64> = HashMap::new();
        let mut buffers: HashMap<String, Vec<PreparedItem>> = HashMap::new();
        let mut stats: HashMap<String, BatchResult> = HashMap::new();

        for row in &rows {
            fetched += 1;
            let name = row
                .get("source")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if name.is_empty() {
                return Err(DbsError::Config(format!(
                    "{}: row {fetched} has no source name",
                    path.display()
                )));
            }
            let item = prepared_item_from_row(row, &format!("{}: row {fetched}", path.display()))?;
            *seen.entry(name.clone()).or_insert(0) += 1;
            if dry_run {
                continue;
            }
            if !records.contains_key(&name) {
                let existing = match self.storage.get_source(&name)? {
                    Some(r) => r,
                    None => {
                        let stype = row
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        self.storage.upsert_source(
                            &name,
                            stype,
                            &format!("restored:{stype}"),
                            "{}",
                            1,
                        )?
                    }
                };
                let run_id =
                    self.storage
                        .begin_run(existing.id, &existing.plugin_id, "restore", None)?;
                records.insert(name.clone(), existing);
                runs.insert(name.clone(), run_id);
                buffers.insert(name.clone(), Vec::new());
                stats.insert(name.clone(), BatchResult::default());
            }
            buffers.get_mut(&name).unwrap().push(item);
            if buffers[&name].len() >= 500 {
                let batch = std::mem::take(buffers.get_mut(&name).unwrap());
                let res =
                    self.storage
                        .upsert_items(records[&name].id, runs[&name], &batch, false, 0)?;
                stats.get_mut(&name).unwrap().merge(&res);
            }
        }

        for (name, batch) in buffers.iter_mut() {
            if batch.is_empty() {
                continue;
            }
            let res = self
                .storage
                .upsert_items(records[name].id, runs[name], batch, false, 0)?;
            stats.get_mut(name).unwrap().merge(&res);
        }

        for (name, run_id) in &runs {
            self.storage.finish_run(
                *run_id,
                run_status_str(RunStatus::Success),
                &stats[name],
                *seen.get(name).unwrap_or(&0),
                None,
                None,
                &[],
            )?;
        }

        let mut totals = BatchResult::default();
        for st in stats.values() {
            totals.merge(st);
        }

        if let Some(expected) = manifest
            .as_ref()
            .and_then(|m| m.get("counts"))
            .and_then(|c| c.get("items"))
            .and_then(|v| v.as_u64())
        {
            if expected != fetched {
                warnings.push(format!(
                    "manifest says {expected} item(s) but the bundle held {fetched}"
                ));
            }
        }

        let mut sources: Vec<String> = seen.keys().cloned().collect();
        sources.sort();

        Ok(RestoreReport {
            path: path.display().to_string(),
            dry_run,
            sources,
            fetched,
            created: totals.created,
            updated: totals.updated,
            unchanged: totals.unchanged,
            deleted: totals.deleted,
            revisions_skipped,
            media_skipped,
            warnings,
        })
    }

    /// Integrity checks on the database and per-source state. Mirrors
    /// the reference's `BackupService.verify`.
    ///
    /// Archive-bundle checksum verification is a separate entry point
    /// ([`crate::restore::verify_archive`], #59) — the reference's CLI
    /// calls it directly for `dbs verify --archive` rather than through
    /// this method, and this port follows the same split.
    pub fn verify(&self, name: Option<&str>) -> Result<VerifyReport, DbsError> {
        let mut issues: Vec<VerifyIssue> = Vec::new();

        let integrity = self.storage.integrity_check()?;
        if integrity != "ok" {
            issues.push(VerifyIssue {
                source: "(database)".to_string(),
                kind: "integrity".to_string(),
                detail: integrity,
            });
        }

        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => self.config.sources.keys().cloned().collect(),
        };
        for n in &names {
            let Some(source) = self.storage.get_source(n)? else {
                continue;
            };
            if let Err(e) = self.storage.load_cursor(source.id) {
                issues.push(VerifyIssue {
                    source: n.clone(),
                    kind: "cursor".to_string(),
                    detail: format!("unparseable cursor: {e}"),
                });
            }
            for run in self.storage.recent_runs(Some(source.id), 50)? {
                if run.get("status").and_then(|v| v.as_str()) == Some("running") {
                    let run_id = run.get("id").and_then(|v| v.as_i64()).unwrap_or_default();
                    issues.push(VerifyIssue {
                        source: n.clone(),
                        kind: "orphan_run".to_string(),
                        detail: format!("run {run_id} stuck 'running'"),
                    });
                }
            }
        }

        Ok(VerifyReport {
            ok: issues.is_empty(),
            issues,
        })
    }

    /// Runs `query` through `format`'s [`Exporter`](crate::export::Exporter)
    /// and atomically writes the result to `path` (write to a sibling
    /// `.tmp` file, then rename — a crash mid-export never leaves a
    /// half-written file at `path`). With `encrypt_passphrase: Some(_)`,
    /// the exporter writes through an [`EncryptingWriter`] instead of
    /// straight to the file — same atomicity, since encryption happens
    /// inside the tmp-file-then-rename span, not after it. Mirrors the
    /// reference's `BackupService.export`; passphrase *resolution*
    /// (`--passphrase-env` / `.env` / the process environment) is the
    /// caller's job via [`crate::crypto::resolve_passphrase`], matching
    /// how this method also doesn't own `ExportQuery` construction.
    ///
    /// Pulled forward from #70 (the CLI-facing `dbs export*` wiring):
    /// [`crate::notes_export::export_notes`]/`export_wiki_dir` (#61)
    /// cannot exist without *some* way to turn a query into a written
    /// file, and every exporter issue (#51-#58) already lands
    /// `Exporter`/`ExportQuery` — this method is the missing link
    /// between them and `Storage`, not CLI argument parsing (still
    /// #70's own scope).
    pub fn export(
        &self,
        query: &ExportQuery,
        format: &str,
        path: &Path,
        encrypt_passphrase: Option<&str>,
    ) -> Result<ExportResult, DbsError> {
        let exporter = get_exporter(format)?;
        let source = self.build_export_source(query)?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    DbsError::Storage(format!("failed to create export directory: {e}"))
                })?;
            }
        }
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = path.with_file_name(tmp_name);

        let mut result = {
            let file = std::fs::File::create(&tmp_path).map_err(|e| {
                DbsError::Storage(format!("failed to create export temp file: {e}"))
            })?;
            match encrypt_passphrase {
                Some(passphrase) => {
                    let mut writer = EncryptingWriter::new(file, passphrase)?;
                    let result = exporter.write(&source, &mut writer, query)?;
                    let mut file = writer.finish()?;
                    file.flush().map_err(|e| {
                        DbsError::Storage(format!("failed to flush export file: {e}"))
                    })?;
                    result
                }
                None => {
                    let mut file = file;
                    let result = exporter.write(&source, &mut file, query)?;
                    file.flush().map_err(|e| {
                        DbsError::Storage(format!("failed to flush export file: {e}"))
                    })?;
                    result
                }
            }
        };
        std::fs::rename(&tmp_path, path)
            .map_err(|e| DbsError::Storage(format!("failed to finalize export file: {e}")))?;
        result.path = Some(path.display().to_string());
        Ok(result)
    }

    /// Eagerly collects `query`'s matching rows from storage into an
    /// in-memory [`ExportSource`] — the `Exporter` trait's `items()`
    /// etc. are infallible, so any storage error surfaces here instead,
    /// before a single byte is written.
    fn build_export_source(&self, query: &ExportQuery) -> Result<InMemoryExportSource, DbsError> {
        let items: Vec<ItemRow> = self.storage.iter_items(query)?.collect();
        let revisions: Vec<ItemRow> = if query.include_revisions {
            self.storage.iter_revisions(query)?.collect()
        } else {
            Vec::new()
        };
        let media_blobs: Vec<ItemRow> = self.storage.iter_media_blobs(query)?.collect();
        Ok(InMemoryExportSource {
            items,
            revisions,
            media_blobs,
            manifest: self.export_manifest_row(),
            profiles: self.export_profiles(),
        })
    }

    /// `tool`/`generated_at`/`db_schema_version`/`connector_schema_versions`
    /// — the base manifest fields every zip exporter (obsidian/wiki/
    /// archive) merges its own `query`/`counts` on top of. Mirrors the
    /// reference's `BackupService._manifest`, minus `tool_version`/
    /// `git_sha` (no build-metadata/VCS-introspection equivalent wired
    /// up yet in this port).
    fn export_manifest_row(&self) -> ItemRow {
        let mut manifest = ItemRow::new();
        manifest.insert(
            "tool".to_string(),
            serde_json::Value::String("rusty_dbs".to_string()),
        );
        manifest.insert(
            "generated_at".to_string(),
            serde_json::Value::String(iso_z(Utc::now())),
        );
        manifest.insert(
            "db_schema_version".to_string(),
            serde_json::Value::from(SCHEMA_VERSION),
        );
        let connector_schema_versions: serde_json::Map<String, serde_json::Value> = self
            .registry
            .all()
            .iter()
            .map(|rc| {
                (
                    rc.type_.clone(),
                    serde_json::Value::from(rc.handshake.schema_version),
                )
            })
            .collect();
        manifest.insert(
            "connector_schema_versions".to_string(),
            serde_json::Value::Object(connector_schema_versions),
        );
        manifest
    }

    /// Connector default, then the source's `[sources.NAME.export]`
    /// block, field by field. Mirrors the reference's
    /// `BackupService._export_profiles`.
    pub fn export_profiles(&self) -> HashMap<String, ExportProfile> {
        let mut profiles = HashMap::new();
        for (name, sc) in &self.config.sources {
            let default = self
                .registry
                .get(&sc.type_)
                .and_then(|rc| rc.handshake.export_profile.clone());
            let profile =
                resolve_export_profile(default.as_ref(), sc.export.as_ref()).unwrap_or_default();
            profiles.insert(name.clone(), profile);
        }
        profiles
    }
}

/// An in-memory [`ExportSource`] populated eagerly from [`Storage`] by
/// [`BackupService::build_export_source`].
struct InMemoryExportSource {
    items: Vec<ItemRow>,
    revisions: Vec<ItemRow>,
    media_blobs: Vec<ItemRow>,
    manifest: ItemRow,
    profiles: HashMap<String, ExportProfile>,
}

impl ExportSource for InMemoryExportSource {
    fn items(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
        Box::new(self.items.iter().cloned())
    }
    fn revisions(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
        Box::new(self.revisions.iter().cloned())
    }
    fn media_blobs(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
        Box::new(self.media_blobs.iter().cloned())
    }
    fn manifest(&self) -> ItemRow {
        self.manifest.clone()
    }
    fn profiles(&self) -> HashMap<String, ExportProfile> {
        self.profiles.clone()
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
        /// `source_id -> (external_id -> content_hash)`, for a real
        /// (not `unimplemented!()`) `upsert_items` the restore tests
        /// need to exercise created/updated/unchanged classification.
        items_by_source: HashMap<i64, HashMap<String, String>>,
        /// `integrity_check`'s canned return value — defaults to `""`
        /// (an unrealistic sentinel; verify tests set it explicitly to
        /// `"ok"` or a failure string, since no test but verify's own
        /// cares what this returns).
        integrity: String,
        /// When set, `load_cursor` for this source id errors instead
        /// of returning normally — how verify tests exercise the
        /// "unparseable cursor" issue path.
        unparseable_cursor: Option<i64>,
        /// Canned rows for `iter_items`/`iter_revisions`/
        /// `iter_media_blobs` — export tests populate these directly
        /// rather than modeling real query filtering.
        export_items: Vec<ItemRow>,
        export_revisions: Vec<ItemRow>,
        export_media_blobs: Vec<ItemRow>,
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
                .map(|(id, r)| {
                    let mut row = ItemRow::new();
                    row.insert("id".to_string(), serde_json::Value::from(*id));
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
            source_id: i64,
            _run_id: i64,
            items: &[crate::storage::PreparedItem],
            _store_media: bool,
            _max_media_bytes: u64,
        ) -> Result<BatchResult, DbsError> {
            let mut result = BatchResult::default();
            let store = self.items_by_source.entry(source_id).or_default();
            for item in items {
                match store.get(&item.external_id) {
                    Some(existing_hash) if existing_hash == &item.content_hash => {
                        result.unchanged += 1;
                    }
                    Some(_) => {
                        result.updated += 1;
                        store.insert(item.external_id.clone(), item.content_hash.clone());
                    }
                    None => {
                        result.created += 1;
                        store.insert(item.external_id.clone(), item.content_hash.clone());
                    }
                }
            }
            Ok(result)
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
            if self.unparseable_cursor == Some(source_id) {
                return Err(DbsError::Storage("cursor is not valid JSON".to_string()));
            }
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
            Ok(Box::new(self.export_items.iter().cloned()))
        }
        fn iter_revisions<'a>(
            &'a self,
            _: &crate::storage::ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            Ok(Box::new(self.export_revisions.iter().cloned()))
        }
        fn iter_media_blobs<'a>(
            &'a self,
            _: &crate::storage::ExportQuery,
        ) -> Result<Box<dyn Iterator<Item = ItemRow> + 'a>, DbsError> {
            Ok(Box::new(self.export_media_blobs.iter().cloned()))
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
            Ok(self.integrity.clone())
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
                auth_capture: None,
            },
            command: std::path::PathBuf::from("dbs-connector-test"),
            args: Vec::new(),
        }
    }

    struct ScriptedRunner {
        result: std::sync::Mutex<Result<ConnectorRunOutcome, String>>,
    }

    impl ScriptedRunner {
        fn success(stats: BatchResult, items_seen: u64) -> Self {
            Self {
                result: std::sync::Mutex::new(Ok(ConnectorRunOutcome {
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
                result: std::sync::Mutex::new(Err(msg.to_string())),
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
            match &*self.result.lock().unwrap() {
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

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<ProgressEvent>>,
    }

    impl CollectingSink {
        fn events(&self) -> Vec<ProgressEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ProgressSink for CollectingSink {
        fn emit(&self, event: &ProgressEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    /// Forwards to `inner`, then cancels `cancel` the moment a
    /// `SourceDone` event is seen — how the cancellation tests below
    /// simulate Ctrl+C landing right after one source finishes.
    struct CancelAfterSourceDone<'a> {
        inner: &'a CollectingSink,
        cancel: CancelToken,
    }

    impl ProgressSink for CancelAfterSourceDone<'_> {
        fn emit(&self, event: &ProgressEvent) {
            self.inner.emit(event);
            if event.phase == ProgressPhase::SourceDone {
                self.cancel.cancel();
            }
        }
    }

    #[test]
    fn backup_source_emits_source_start_then_source_done() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 3);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let sink = CollectingSink::default();
        let opts = BackupSourceOptions {
            on_progress: Some(&sink),
            ..BackupSourceOptions::default()
        };
        let result = service.backup_source("a", &opts).unwrap();

        let events = sink.events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].phase, ProgressPhase::SourceStart);
        assert_eq!(events[0].source, "a");
        assert!(events[0].result.is_none());
        assert_eq!(events[1].phase, ProgressPhase::SourceDone);
        assert_eq!(events[1].fetched, 3);
        assert_eq!(events[1].result.as_ref().unwrap().status, result.status);
    }

    #[test]
    fn backup_source_emits_no_progress_for_a_disabled_source() {
        // Nothing is actually dispatched for a disabled source (it returns
        // before the registry lookup), so there's no SourceStart/SourceDone
        // to report — an honest silence, not a missed event.
        let mut sources = HashMap::new();
        let mut disabled = test_source_config("a", "raindrop");
        disabled.enabled = false;
        sources.insert("a".to_string(), disabled);
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let sink = CollectingSink::default();
        let opts = BackupSourceOptions {
            on_progress: Some(&sink),
            ..BackupSourceOptions::default()
        };
        service.backup_source("a", &opts).unwrap();
        assert!(sink.events().is_empty());
    }

    #[test]
    fn backup_all_frames_progress_events_with_source_index_and_total() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let sink = CollectingSink::default();
        let opts = BackupAllOptions {
            on_progress: Some(&sink),
            ..BackupAllOptions::default()
        };
        service.backup_all(&opts).unwrap();

        let starts: Vec<_> = sink
            .events()
            .into_iter()
            .filter(|e| e.phase == ProgressPhase::SourceStart)
            .collect();
        assert_eq!(starts.len(), 2, "{starts:?}");
        // Sources run in name-sorted order: "a" then "b".
        assert_eq!(starts[0].source, "a");
        assert_eq!(starts[0].source_index, Some(1));
        assert_eq!(starts[0].source_total, Some(2));
        assert_eq!(starts[1].source, "b");
        assert_eq!(starts[1].source_index, Some(2));
        assert_eq!(starts[1].source_total, Some(2));
    }

    #[test]
    fn backup_all_cancel_token_stops_before_the_next_source() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));
        sources.insert("c".to_string(), test_source_config("c", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let sink = CollectingSink::default();
        let cancel = CancelToken::new();
        let cancelling_sink = CancelAfterSourceDone {
            inner: &sink,
            cancel: cancel.clone(),
        };
        let opts = BackupAllOptions {
            on_progress: Some(&cancelling_sink),
            cancel: Some(cancel),
            ..BackupAllOptions::default()
        };
        // "a" sorts first: it runs and its SourceDone cancels the token
        // before "b" can start.
        let results = service.backup_all(&opts).unwrap();
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].source, "a");
    }

    #[test]
    fn backup_all_without_cancel_runs_every_source_as_usual() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let results = service.backup_all(&BackupAllOptions::default()).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn backup_all_parallel_cancel_token_stops_dequeuing_new_work() {
        // A real, file-backed SqliteStorage — the only kind Storage::spawn
        // can serve workers from — with a single worker so cancellation
        // timing is deterministic: "a" (name-sorted first) runs, its
        // SourceDone cancels the token, and no further source starts.
        // Every started run still reaches finish_run (backup_source's own
        // invariant), so storage is left consistent, not half-committed.
        let db_path = parallel_test_db_path("cancel");
        std::fs::remove_file(&db_path).ok();
        let mut storage =
            crate::storage::sqlite_storage::SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();

        let mut sources = HashMap::new();
        for name in ["a", "b", "c", "d"] {
            sources.insert(name.to_string(), test_source_config(name, "raindrop"));
        }
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let sink = CollectingSink::default();
        let cancel = CancelToken::new();
        let cancelling_sink = CancelAfterSourceDone {
            inner: &sink,
            cancel: cancel.clone(),
        };
        let opts = BackupAllOptions {
            parallel: Some(1),
            on_progress: Some(&cancelling_sink),
            cancel: Some(cancel),
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].source, "a");

        // Storage consistency: the one run that did start reached a
        // terminal status, not left dangling as "running".
        let source = storage.get_source("a").unwrap().unwrap();
        let recent = storage.recent_runs(Some(source.id), 1).unwrap();
        let status = recent[0].get("status").and_then(|v| v.as_str()).unwrap();
        assert_ne!(status, "running");

        storage.close();
        std::fs::remove_file(&db_path).ok();
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
    fn backup_all_only_due_includes_every_never_run_source() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            only_due: true,
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        let mut names: Vec<&str> = results.iter().map(|r| r.source.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn backup_all_only_due_with_no_enabled_sources_returns_empty() {
        let mut sources = HashMap::new();
        let mut disabled = test_source_config("a", "raindrop");
        disabled.enabled = false;
        sources.insert("a".to_string(), disabled);

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            only_due: true,
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn backup_all_parallel_falls_back_to_sequential_when_storage_cannot_spawn() {
        // FakeStorage doesn't override `spawn` (defaults to `None`, like
        // an in-memory SQLite database), so a `parallel` request above 1
        // must still produce correct results via the sequential fallback.
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            parallel: Some(4),
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.status == RunStatus::Success));
    }

    #[test]
    fn backup_all_parallel_of_one_is_equivalent_to_sequential() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "raindrop"));

        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            parallel: Some(1),
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.status == RunStatus::Success));
    }

    fn parallel_test_db_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dbs-core-backup-all-parallel-{label}-{}.sqlite3",
            std::process::id()
        ))
    }

    #[test]
    fn backup_all_parallel_runs_multiple_sources_concurrently() {
        // A real, file-backed SqliteStorage — the only kind `spawn()`
        // returns `Some` for — actually exercises the thread pool
        // instead of the sequential fallback every other test here
        // takes via FakeStorage.
        let db_path = parallel_test_db_path("multi");
        std::fs::remove_file(&db_path).ok();
        let mut storage =
            crate::storage::sqlite_storage::SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();

        let mut sources = HashMap::new();
        for name in ["a", "b", "c", "d"] {
            sources.insert(name.to_string(), test_source_config(name, "raindrop"));
        }
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            parallel: Some(4),
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|r| r.status == RunStatus::Success));
        let mut names: Vec<&str> = results.iter().map(|r| r.source.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c", "d"]);

        storage.close();
        std::fs::remove_file(&db_path).ok();
    }

    #[test]
    fn backup_all_parallel_continue_on_error_isolates_one_sources_failure() {
        let db_path = parallel_test_db_path("isolate-failure");
        std::fs::remove_file(&db_path).ok();
        let mut storage =
            crate::storage::sqlite_storage::SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();

        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        // "b" has no registered connector, so backup_source errors for it.
        sources.insert("b".to_string(), test_source_config("b", "unregistered"));
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 1);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let opts = BackupAllOptions {
            parallel: Some(2),
            ..BackupAllOptions::default()
        };
        let results = service.backup_all(&opts).unwrap();
        assert_eq!(results.len(), 2);
        let b_result = results.iter().find(|r| r.source == "b").unwrap();
        assert_eq!(b_result.status, RunStatus::Failed);
        let a_result = results.iter().find(|r| r.source == "a").unwrap();
        assert_eq!(a_result.status, RunStatus::Success);

        storage.close();
        std::fs::remove_file(&db_path).ok();
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

    // -- restore --------------------------------------------------------

    fn restore_temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-service-restore-test-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn restore_item_row(external_id: &str, source: &str, content_hash: &str) -> serde_json::Value {
        serde_json::json!({
            "source": source,
            "external_id": external_id,
            "item_kind": "bookmark",
            "title": "hello",
            "content_hash": content_hash,
            "raw": {"a": 1},
        })
    }

    fn write_ndjson_bundle(path: &std::path::Path, rows: &[serde_json::Value]) {
        use std::io::Write;
        let mut file = std::fs::File::create(path).unwrap();
        for row in rows {
            writeln!(file, "{}", serde_json::to_string(row).unwrap()).unwrap();
        }
    }

    fn write_archive_bundle(
        path: &std::path::Path,
        rows: &[serde_json::Value],
        manifest_extra: serde_json::Value,
        tamper_checksum: bool,
    ) {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::{CompressionMethod, ZipWriter};

        let file = std::fs::File::create(path).unwrap();
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut zf = ZipWriter::new(file);
        let mut text = String::new();
        for row in rows {
            text.push_str(&serde_json::to_string(row).unwrap());
            text.push('\n');
        }
        zf.start_file("items/raindrop.ndjson", options).unwrap();
        zf.write_all(text.as_bytes()).unwrap();

        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let digest = if tamper_checksum {
            "0".repeat(64)
        } else {
            format!("{:x}", hasher.finalize())
        };
        let mut manifest = manifest_extra;
        manifest["checksums"] = serde_json::json!({"items/raindrop.ndjson": digest});
        zf.start_file("manifest.json", options).unwrap();
        zf.write_all(serde_json::to_vec_pretty(&manifest).unwrap().as_slice())
            .unwrap();
        zf.finish().unwrap();
    }

    #[test]
    fn restore_happy_path_creates_a_source_and_upserts_items() {
        let dir = restore_temp_dir("happy-path");
        let path = dir.join("export.ndjson");
        write_ndjson_bundle(
            &path,
            &[
                restore_item_row("e1", "raindrop", "h1"),
                restore_item_row("e2", "raindrop", "h2"),
            ],
        );

        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let report = service.restore(&path, false, None).unwrap();
        assert_eq!(report.fetched, 2);
        assert_eq!(report.created, 2);
        assert_eq!(report.sources, vec!["raindrop".to_string()]);
        assert!(!report.dry_run);
        assert!(storage.sources.contains_key("raindrop"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_dry_run_reports_without_writing() {
        let dir = restore_temp_dir("dry-run");
        let path = dir.join("export.ndjson");
        write_ndjson_bundle(&path, &[restore_item_row("e1", "raindrop", "h1")]);

        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let report = service.restore(&path, true, None).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.fetched, 1);
        assert_eq!(report.created, 0);
        assert!(storage.sources.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_rejects_a_bundle_with_a_newer_schema_version() {
        let dir = restore_temp_dir("schema-mismatch");
        let path = dir.join("bundle.zip");
        write_archive_bundle(
            &path,
            &[restore_item_row("e1", "raindrop", "h1")],
            serde_json::json!({"db_schema_version": SCHEMA_VERSION + 1}),
            false,
        );

        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service.restore(&path, false, None).unwrap_err();
        assert!(err.to_string().contains("newer dbs"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_rejects_a_corrupt_checksum_archive() {
        let dir = restore_temp_dir("corrupt-checksum");
        let path = dir.join("bundle.zip");
        write_archive_bundle(
            &path,
            &[restore_item_row("e1", "raindrop", "h1")],
            serde_json::json!({"db_schema_version": 1}),
            true,
        );

        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service.restore(&path, false, None).unwrap_err();
        assert!(err.to_string().contains("integrity verification"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_decrypts_an_encrypted_bundle_before_restoring() {
        use crate::crypto::EncryptingWriter;
        use std::io::Write;

        let dir = restore_temp_dir("encrypted");
        let path = dir.join("export.ndjson.enc");
        let row = restore_item_row("e1", "raindrop", "h1");
        let mut text = serde_json::to_string(&row).unwrap();
        text.push('\n');
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = EncryptingWriter::new(file, "hunter2").unwrap();
        writer.write_all(text.as_bytes()).unwrap();
        writer.finish().unwrap();

        let mut secret_store = HashMap::new();
        secret_store.insert(
            crate::crypto::DEFAULT_PASSPHRASE_ENV.to_string(),
            "hunter2".to_string(),
        );

        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);

        let report = service.restore(&path, false, Some(&secret_store)).unwrap();
        assert_eq!(report.fetched, 1);
        assert_eq!(report.created, 1);
        // The reported path is the file the caller named, not the temp
        // plaintext used internally to decrypt it.
        assert_eq!(report.path, path.display().to_string());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_errors_for_a_missing_file() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let mut service = BackupService::new(&mut storage, &config, &registry, &runner);
        let err = service
            .restore(
                std::path::Path::new("/no/such/dbs-restore-fixture"),
                false,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("no such file"));
    }

    // -- verify -----------------------------------------------------------

    #[test]
    fn verify_reports_ok_for_a_clean_database_with_no_sources() {
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let report = service.verify(None).unwrap();
        assert!(report.ok);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn verify_flags_a_corrupted_database() {
        let mut storage = FakeStorage {
            integrity: "database disk image is malformed".to_string(),
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let report = service.verify(None).unwrap();
        assert!(!report.ok);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].source, "(database)");
        assert_eq!(report.issues[0].kind, "integrity");
        assert_eq!(report.issues[0].detail, "database disk image is malformed");
    }

    #[test]
    fn verify_flags_an_unparseable_cursor() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        storage
            .upsert_source("a", "raindrop", "raindrop", "{}", 1)
            .unwrap();
        storage.unparseable_cursor = Some(1);
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let report = service.verify(Some("a")).unwrap();
        assert!(!report.ok);
        assert_eq!(report.issues[0].source, "a");
        assert_eq!(report.issues[0].kind, "cursor");
    }

    #[test]
    fn verify_flags_an_orphaned_running_run() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let source = storage
            .upsert_source("a", "raindrop", "raindrop", "{}", 1)
            .unwrap();
        storage
            .begin_run(source.id, "raindrop", "incremental", None)
            .unwrap();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let report = service.verify(Some("a")).unwrap();
        assert!(!report.ok);
        assert_eq!(report.issues[0].source, "a");
        assert_eq!(report.issues[0].kind, "orphan_run");
        assert!(report.issues[0].detail.contains("stuck 'running'"));
    }

    #[test]
    fn verify_skips_a_name_that_does_not_match_any_source() {
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let report = service.verify(Some("missing")).unwrap();
        assert!(report.ok);
    }

    // -- export -------------------------------------------------------------

    fn export_item_row(source: &str, external_id: &str, title: &str) -> ItemRow {
        let mut row = ItemRow::new();
        row.insert(
            "source".to_string(),
            serde_json::Value::String(source.to_string()),
        );
        row.insert(
            "external_id".to_string(),
            serde_json::Value::String(external_id.to_string()),
        );
        row.insert(
            "title".to_string(),
            serde_json::Value::String(title.to_string()),
        );
        row
    }

    #[test]
    fn export_writes_a_file_atomically_and_reports_its_path() {
        let dir = restore_temp_dir("export-json");
        let path = dir.join("export.json");

        let mut storage = FakeStorage {
            export_items: vec![export_item_row("raindrop", "e1", "hello")],
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let result = service
            .export(&crate::storage::ExportQuery::default(), "json", &path, None)
            .unwrap();
        assert_eq!(result.item_count, 1);
        assert_eq!(result.path.as_deref(), Some(path.to_str().unwrap()));
        assert!(path.is_file());
        // No leftover temp file once the rename completes.
        assert!(!path.with_file_name("export.json.tmp").exists());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"external_id\": \"e1\""));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_with_a_passphrase_produces_an_encrypted_file_that_decrypts_to_the_plain_export() {
        let dir = restore_temp_dir("export-encrypted");
        let plain_path = dir.join("export.ndjson");
        let enc_path = dir.join("export.ndjson.enc");

        let mut storage = FakeStorage {
            export_items: vec![export_item_row("raindrop", "e1", "hello")],
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        service
            .export(
                &crate::storage::ExportQuery::default(),
                "ndjson",
                &plain_path,
                None,
            )
            .unwrap();
        let result = service
            .export(
                &crate::storage::ExportQuery::default(),
                "ndjson",
                &enc_path,
                Some("hunter2"),
            )
            .unwrap();
        assert_eq!(result.item_count, 1);
        assert!(crate::crypto::is_encrypted(&enc_path));

        let decrypted_path = dir.join("roundtrip.ndjson");
        crate::crypto::decrypt_file(&enc_path, &decrypted_path, "hunter2").unwrap();
        assert_eq!(
            std::fs::read_to_string(&plain_path).unwrap(),
            std::fs::read_to_string(&decrypted_path).unwrap(),
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_profiles_applies_a_source_level_override_over_the_connector_default() {
        let mut sc = test_source_config("a", "raindrop");
        sc.export = Some(crate::export_profile::ExportProfileOverride {
            enabled: Some(false),
            item_kinds: None,
            group_by: None,
            body_from: None,
            page_per: None,
        });
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), sc);
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let profiles = service.export_profiles();
        assert!(!profiles["a"].enabled);
    }

    #[test]
    fn list_sources_reports_type_enabled_and_backed_up_state() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut disabled = test_source_config("b", "raindrop");
        disabled.enabled = false;
        sources.insert("b".to_string(), disabled);

        let mut storage = FakeStorage::default();
        // "a" has actually been backed up before; "b" never has.
        storage
            .upsert_source("a", "raindrop", "test:raindrop", "{}", 1)
            .unwrap();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let rows = service.list_sources().unwrap();
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|r| r["name"] == "a").unwrap();
        assert_eq!(a["type"], "raindrop");
        assert_eq!(a["enabled"], true);
        assert_eq!(a["backed_up"], true);
        let b = rows.iter().find(|r| r["name"] == "b").unwrap();
        assert_eq!(b["enabled"], false);
        assert_eq!(b["backed_up"], false);
    }

    #[test]
    fn list_connectors_maps_registered_connectors_to_connector_info() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let infos = service.list_connectors();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].type_, "raindrop");
        assert!(infos[0].is_builtin);
        assert_eq!(infos[0].item_kinds.len(), 1);
        assert_eq!(infos[0].item_kinds[0].name, "item");
    }

    #[test]
    fn check_sources_reports_ok_for_a_registered_type_and_an_error_for_an_unregistered_one() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        sources.insert("b".to_string(), test_source_config("b", "unregistered"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let results: HashMap<String, Option<String>> =
            service.check_sources().into_iter().collect();
        assert_eq!(results["a"], None);
        assert!(results["b"].as_ref().unwrap().contains("unregistered"));
    }

    #[test]
    fn add_source_appends_a_toml_block_for_a_registered_type() {
        let dir = restore_temp_dir("add-source");
        let config_path = dir.join("dbs.toml");
        std::fs::write(&config_path, "[dbs]\ndatabase = \"dbs.sqlite3\"\n").unwrap();

        let mut storage = FakeStorage::default();
        let mut config = test_config(HashMap::new());
        config.source_path = Some(config_path.clone());
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let mut options = HashMap::new();
        options.insert("collection".to_string(), serde_json::Value::from("123"));
        service
            .add_source("a", "raindrop", &options, false, 0, false)
            .unwrap();

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("[sources.a]"), "{text}");
        assert!(text.contains("type = \"raindrop\""), "{text}");
        assert!(text.contains("collection = \"123\""), "{text}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_source_rejects_an_unregistered_connector_type_without_touching_the_file() {
        let dir = restore_temp_dir("add-source-bad-type");
        let config_path = dir.join("dbs.toml");
        std::fs::write(&config_path, "[dbs]\ndatabase = \"dbs.sqlite3\"\n").unwrap();
        let before = std::fs::read_to_string(&config_path).unwrap();

        let mut storage = FakeStorage::default();
        let mut config = test_config(HashMap::new());
        config.source_path = Some(config_path.clone());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service
            .add_source("a", "raindrop", &HashMap::new(), false, 0, false)
            .unwrap_err();
        assert!(matches!(
            err,
            DbsError::Load(crate::errors::ConnectorLoadError::NotFound(_))
        ));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_source_rejects_a_name_that_already_exists() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service
            .add_source("a", "raindrop", &HashMap::new(), false, 0, false)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    fn fake_connector_with_auth_capture(type_name: &str, kind: &str) -> RegisteredConnector {
        let mut rc = fake_connector(type_name, true, true);
        rc.handshake.auth_capture = Some(crate::capabilities::AuthCapture {
            kind: kind.to_string(),
            ..Default::default()
        });
        rc
    }

    #[test]
    fn resolve_capture_target_finds_a_connector_type_directly() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([fake_connector_with_auth_capture(
            "reddit",
            "browser_cookies",
        )]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let (rc, spec) = service.resolve_capture_target("reddit").unwrap();
        assert_eq!(rc.type_, "reddit");
        assert_eq!(spec.kind, "browser_cookies");
    }

    #[test]
    fn resolve_capture_target_falls_back_to_a_configured_source_name() {
        let mut sources = HashMap::new();
        sources.insert(
            "my-reddit".to_string(),
            test_source_config("my-reddit", "reddit"),
        );
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector_with_auth_capture(
            "reddit",
            "browser_session",
        )]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let (rc, spec) = service.resolve_capture_target("my-reddit").unwrap();
        assert_eq!(rc.type_, "reddit");
        assert_eq!(spec.kind, "browser_session");
    }

    #[test]
    fn resolve_capture_target_errors_when_neither_a_connector_nor_source_matches() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service.resolve_capture_target("nonexistent").unwrap_err();
        assert!(err.to_string().contains("no such connector or source"));
    }

    #[test]
    fn resolve_capture_target_errors_when_the_sources_connector_type_is_unregistered() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage::default();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service.resolve_capture_target("a").unwrap_err();
        assert!(matches!(
            err,
            DbsError::Load(crate::errors::ConnectorLoadError::NotFound(_))
        ));
    }

    #[test]
    fn resolve_capture_target_errors_when_the_connector_has_no_auth_capture() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let err = service.resolve_capture_target("raindrop").unwrap_err();
        assert!(err.to_string().contains("no interactive auth capture"));
    }

    fn seed_video_item(
        storage: &mut crate::storage::sqlite_storage::SqliteStorage,
        source: &str,
        source_type: &str,
        external_id: &str,
        video_id: &str,
        list_label: Option<&str>,
    ) {
        let existing = storage
            .upsert_source(source, source_type, source_type, "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(existing.id, source_type, "full", None)
            .unwrap();
        let mut raw = serde_json::Map::new();
        raw.insert("id".to_string(), Value::from(video_id));
        if let Some(label) = list_label {
            raw.insert("list_label".to_string(), Value::from(label));
        }
        let item = PreparedItem {
            external_id: external_id.to_string(),
            item_kind: "video".to_string(),
            title: Some(format!("video {video_id}")),
            url: Some(format!("https://www.youtube.com/watch?v={video_id}")),
            body: None,
            tags: vec![],
            item_created_at: Some(iso_z(Utc::now())),
            item_updated_at: Some(iso_z(Utc::now())),
            content_hash: format!("hash-{external_id}"),
            raw_json: serde_json::to_string(&Value::Object(raw)).unwrap(),
            deleted: false,
            media: Vec::new(),
        };
        storage
            .upsert_items(existing.id, run_id, &[item], false, 0)
            .unwrap();
        storage
            .finish_run(
                run_id,
                "success",
                &BatchResult::default(),
                1,
                None,
                None,
                &[],
            )
            .unwrap();
    }

    fn open_test_storage() -> crate::storage::sqlite_storage::SqliteStorage {
        let mut storage = crate::storage::sqlite_storage::SqliteStorage::open(":memory:").unwrap();
        storage.migrate().unwrap();
        storage
    }

    #[test]
    fn select_youtube_backup_videos_keeps_only_youtube_type_sources() {
        let mut storage = open_test_storage();
        seed_video_item(&mut storage, "yt", "youtube", "yt:v1", "v1", None);
        seed_video_item(&mut storage, "rd", "raindrop", "rd:v2", "v2", None);
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let videos = service
            .select_youtube_backup_videos(None, None, None)
            .unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0]["raw"]["id"], Value::from("v1"));
    }

    #[test]
    fn select_youtube_backup_videos_dedups_the_same_video_across_lists() {
        let mut storage = open_test_storage();
        seed_video_item(
            &mut storage,
            "yt",
            "youtube",
            "yt:watch-later:v1",
            "v1",
            Some("watch-later"),
        );
        seed_video_item(
            &mut storage,
            "yt",
            "youtube",
            "yt:liked:v1",
            "v1",
            Some("liked"),
        );
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let videos = service
            .select_youtube_backup_videos(None, None, None)
            .unwrap();
        assert_eq!(videos.len(), 1);
    }

    #[test]
    fn select_youtube_backup_videos_filters_by_list_label() {
        let mut storage = open_test_storage();
        seed_video_item(
            &mut storage,
            "yt",
            "youtube",
            "yt:watch-later:v1",
            "v1",
            Some("watch-later"),
        );
        seed_video_item(
            &mut storage,
            "yt",
            "youtube",
            "yt:liked:v2",
            "v2",
            Some("liked"),
        );
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let lists = vec!["liked".to_string()];
        let videos = service
            .select_youtube_backup_videos(None, Some(&lists), None)
            .unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0]["raw"]["id"], Value::from("v2"));
    }

    #[test]
    fn select_youtube_backup_videos_truncates_to_limit() {
        let mut storage = open_test_storage();
        for i in 0..5 {
            seed_video_item(
                &mut storage,
                "yt",
                "youtube",
                &format!("yt:v{i}"),
                &format!("v{i}"),
                None,
            );
        }
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let videos = service
            .select_youtube_backup_videos(None, None, Some(2))
            .unwrap();
        assert_eq!(videos.len(), 2);
    }

    #[test]
    fn select_youtube_backup_videos_restricts_to_the_given_source_names() {
        let mut storage = open_test_storage();
        seed_video_item(&mut storage, "a", "youtube", "a:v1", "v1", None);
        seed_video_item(&mut storage, "b", "youtube", "b:v2", "v2", None);
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let sources = vec!["b".to_string()];
        let videos = service
            .select_youtube_backup_videos(Some(&sources), None, None)
            .unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0]["raw"]["id"], Value::from("v2"));
    }

    #[test]
    fn select_youtube_backup_videos_with_no_matches_returns_empty() {
        let mut storage = open_test_storage();
        seed_video_item(&mut storage, "rd", "raindrop", "rd:v1", "v1", None);
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let videos = service
            .select_youtube_backup_videos(None, None, None)
            .unwrap();
        assert!(videos.is_empty());
    }

    #[test]
    fn doctor_reports_database_integrity_ok_on_a_healthy_db() {
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let integrity = checks
            .iter()
            .find(|c| c.name == "database.integrity")
            .unwrap();
        assert_eq!(integrity.status, "ok");
    }

    #[test]
    fn doctor_reports_a_failed_integrity_check() {
        let mut storage = FakeStorage {
            integrity: "database disk image is malformed".to_string(),
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let integrity = checks
            .iter()
            .find(|c| c.name == "database.integrity")
            .unwrap();
        assert_eq!(integrity.status, "fail");
        assert!(integrity.detail.contains("malformed"));
    }

    #[test]
    fn doctor_reports_interrupted_runs_as_a_warning() {
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let source = storage
            .upsert_source("a", "raindrop", "test:raindrop", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "test", "incremental", None)
            .unwrap();
        storage
            .finish_run(
                run_id,
                "interrupted",
                &BatchResult::default(),
                0,
                None,
                None,
                &[],
            )
            .unwrap();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let interrupted = checks
            .iter()
            .find(|c| c.name == "runs.interrupted")
            .unwrap();
        assert_eq!(interrupted.status, "warn");
        assert!(interrupted.detail.contains("1 interrupted"));
    }

    #[test]
    fn doctor_marks_a_disabled_source_ok_without_touching_the_registry() {
        let mut sc = test_source_config("a", "raindrop");
        sc.enabled = false;
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), sc);
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let source_check = checks.iter().find(|c| c.name == "source.a").unwrap();
        assert_eq!(source_check.status, "ok");
        assert_eq!(source_check.detail, "disabled");
    }

    #[test]
    fn doctor_marks_an_unregistered_source_type_as_a_failure() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let source_check = checks.iter().find(|c| c.name == "source.a").unwrap();
        assert_eq!(source_check.status, "fail");
        assert!(source_check.detail.contains("unavailable"));
    }

    fn fake_connector_requiring_auth(secret_keys: &[&str]) -> RegisteredConnector {
        let mut rc = fake_connector("raindrop", true, true);
        rc.handshake.capabilities.requires_auth = true;
        rc.handshake.secret_keys = secret_keys.iter().map(|s| s.to_string()).collect();
        rc
    }

    #[test]
    fn doctor_secrets_check_fails_when_none_of_the_declared_secrets_are_set() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(sources);
        let registry =
            ConnectorRegistry::from_resolved([fake_connector_requiring_auth(&["RAINDROP_TOKEN"])]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let secrets_check = checks
            .iter()
            .find(|c| c.name == "source.a.secrets")
            .unwrap();
        assert_eq!(secrets_check.status, "fail");
        assert!(secrets_check.detail.contains("RAINDROP_TOKEN"));
    }

    #[test]
    fn doctor_secrets_check_passes_when_a_declared_secret_is_set() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(sources);
        let registry =
            ConnectorRegistry::from_resolved([fake_connector_requiring_auth(&["RAINDROP_TOKEN"])]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let mut secret_store = HashMap::new();
        secret_store.insert("RAINDROP_TOKEN".to_string(), "secret-value".to_string());

        let checks = service.doctor(Some(&secret_store));
        let secrets_check = checks
            .iter()
            .find(|c| c.name == "source.a.secrets")
            .unwrap();
        assert_eq!(secrets_check.status, "ok");
        assert!(secrets_check.detail.contains("RAINDROP_TOKEN"));
    }

    #[test]
    fn doctor_vpn_check_is_ok_when_the_guard_is_off_even_without_a_netns() {
        let mut sc = test_source_config("a", "raindrop");
        sc.requires_vpn = true;
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), sc);
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let mut config = test_config(sources);
        config.vpn_guard = crate::config::VpnGuard::Off;
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let vpn_check = checks.iter().find(|c| c.name == "source.a.vpn").unwrap();
        assert_eq!(vpn_check.status, "ok");
        assert!(vpn_check.detail.contains("vpn_guard=off"));
    }

    #[test]
    fn doctor_vpn_check_warns_when_the_netns_is_not_up() {
        let mut sc = test_source_config("a", "raindrop");
        sc.requires_vpn = true;
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), sc);
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let mut config = test_config(sources);
        // Default VpnGuard::Skip plus a netns name that certainly
        // doesn't exist on the test runner.
        config.vpn_netns = "rusty-dbs-test-netns-that-does-not-exist".to_string();
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let vpn_check = checks.iter().find(|c| c.name == "source.a.vpn").unwrap();
        assert_eq!(vpn_check.status, "warn");
        assert!(vpn_check.detail.contains("not up"));
    }

    #[test]
    fn doctor_staleness_warns_when_the_last_success_is_far_past_the_schedule_slack() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        // FakeStorage timestamps every run at a fixed 2026-01-01 —
        // always more than twice the daily slack behind "now" (this
        // session's date context is 2026-08-13), so a committed
        // success run here is deterministically stale.
        let source = storage
            .upsert_source("a", "raindrop", "test:raindrop", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(source.id, "test", "incremental", None)
            .unwrap();
        storage
            .finish_run(
                run_id,
                "success",
                &BatchResult::default(),
                0,
                None,
                None,
                &[],
            )
            .unwrap();
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        let staleness = checks
            .iter()
            .find(|c| c.name == "source.a.staleness")
            .unwrap();
        assert_eq!(staleness.status, "warn");
    }

    #[test]
    fn doctor_no_staleness_check_for_a_source_that_has_never_run() {
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), test_source_config("a", "raindrop"));
        let mut storage = FakeStorage {
            integrity: "ok".to_string(),
            ..Default::default()
        };
        let config = test_config(sources);
        let registry = ConnectorRegistry::from_resolved([fake_connector("raindrop", true, true)]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let checks = service.doctor(None);
        assert!(!checks.iter().any(|c| c.name == "source.a.staleness"));
    }

    #[test]
    fn export_errors_on_an_unknown_format() {
        let mut storage = FakeStorage::default();
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let dir = restore_temp_dir("export-bad-format");
        let err = service
            .export(
                &crate::storage::ExportQuery::default(),
                "bogus",
                &dir.join("out.bin"),
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("bogus"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_obsidian_includes_a_manifest_with_schema_version() {
        let dir = restore_temp_dir("export-obsidian-manifest");
        let path = dir.join("bundle.zip");

        let mut storage = FakeStorage {
            export_items: vec![export_item_row("raindrop", "e1", "hello")],
            ..Default::default()
        };
        let config = test_config(HashMap::new());
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = ScriptedRunner::success(BatchResult::default(), 0);
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        service
            .export(
                &crate::storage::ExportQuery::default(),
                "obsidian",
                &path,
                None,
            )
            .unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut manifest_text = String::new();
        std::io::Read::read_to_string(
            &mut archive.by_name("manifest.json").unwrap(),
            &mut manifest_text,
        )
        .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["db_schema_version"], SCHEMA_VERSION);
        assert_eq!(manifest["tool"], "rusty_dbs");

        std::fs::remove_dir_all(&dir).ok();
    }
}
