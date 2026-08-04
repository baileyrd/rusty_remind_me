//! The background sync cycle: push the local outbox to the hub, pull the
//! hub's changes back, prune what's now safe to drop. Runs in its own
//! daemon-style thread on [`SYNC_INTERVAL_ENV`], matching the reference's
//! own `sync_loop`/`start_sync_thread` — a hub outage or a bad response
//! must never crash this thread or the process, only show up in
//! [`SyncWorker::status`].

use super::{prune_outbox, pull_remote, push_outbox};
use crate::Database;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// `sync_log`/`sync_sends`' `remote_id` for the single configured hub.
/// Every other remote this worker syncs with (via [`super::discover_peers`])
/// is keyed by its own `node_id` instead.
pub const HUB_REMOTE_ID: &str = "hub";

const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

fn sync_interval() -> Duration {
    let secs = std::env::var(super::SYNC_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(super::DEFAULT_SYNC_INTERVAL_SECS);
    Duration::from_secs(secs)
}

#[derive(Debug, Default)]
struct WorkerState {
    cycles: usize,
    last_cycle_at: Option<String>,
    last_error: Option<String>,
}

pub struct SyncWorker {
    state: Arc<Mutex<WorkerState>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SyncWorker {
    /// `None` when sync isn't enabled — matching the reference's own
    /// `if SYNC_ENABLED: start_sync_thread()` gating exactly.
    pub fn from_env(db: Arc<Database>) -> Option<Self> {
        if !super::sync_enabled() {
            return None;
        }
        let hub_url = super::configured_hub_url();
        let secret = super::configured_sync_secret();
        let node_id = super::configured_node_id();
        let interval = sync_interval();

        let state = Arc::new(Mutex::new(WorkerState::default()));
        let thread_state = Arc::clone(&state);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("sync-worker".to_string())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Relaxed) {
                    run_one_cycle(&db, &hub_url, &secret, &node_id, &thread_state);

                    let mut waited = Duration::ZERO;
                    while waited < interval && !thread_shutdown.load(Ordering::Relaxed) {
                        std::thread::sleep(SHUTDOWN_POLL);
                        waited += SHUTDOWN_POLL;
                    }
                }
            })
            .ok()?;

        Some(Self {
            state,
            shutdown,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// The worker's state as it stands in **this process**.
    ///
    /// Prefer [`SyncWorker::status_against`] wherever a database connection is
    /// available: without one, a failure this process saw cannot be checked
    /// against the shared watermarks, so a recovered sync still reads as
    /// failing.
    pub fn status(&self) -> SyncWorkerStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        SyncWorkerStatus {
            enabled: true,
            running: self.is_running(),
            cycles: state.cycles,
            last_cycle_at: state.last_cycle_at.clone(),
            last_error: state.last_error.clone(),
            superseded_error: None,
        }
    }

    /// The worker's state, with a failure the shared `sync_log` watermarks
    /// have moved past demoted to `superseded_error`.
    pub fn status_against(&self, conn: &Connection) -> SyncWorkerStatus {
        let mut status = self.status();
        if status.last_error.is_some()
            && superseded(conn, status.last_cycle_at.as_deref()).unwrap_or(false)
        {
            status.superseded_error = status.last_error.take();
        }
        status
    }
}

impl Drop for SyncWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Push and pull (all four tables) against one remote. One push drains the
/// whole outbox regardless of which table's trigger wrote a given row --
/// memories and graph-table rows go together, in one pass; pulls are
/// per-table, each with its own cursor. Returns the first error
/// encountered, if any -- one remote's failure never stops the rest of the
/// tables from being tried against it, and never stops the cycle from
/// moving on to the next remote.
fn sync_with_remote(
    conn: &Connection,
    url: &str,
    secret: &str,
    node_id: &str,
    remote_id: &str,
) -> Option<String> {
    let mut error = None;
    if let Err(e) = push_outbox(conn, url, secret, node_id, remote_id) {
        error = Some(format!("push to {remote_id} failed: {e}"));
    }
    if let Err(e) = pull_remote(conn, url, secret, node_id, remote_id) {
        error.get_or_insert_with(|| format!("pull from {remote_id} failed: {e}"));
    }
    if let Err(e) = super::pull_entities(conn, url, secret, node_id, remote_id) {
        error.get_or_insert_with(|| format!("pull entities from {remote_id} failed: {e}"));
    }
    if let Err(e) = super::pull_links(conn, url, secret, node_id, remote_id) {
        error.get_or_insert_with(|| format!("pull links from {remote_id} failed: {e}"));
    }
    if let Err(e) = super::pull_entity_relations(conn, url, secret, node_id, remote_id) {
        error.get_or_insert_with(|| format!("pull entity relations from {remote_id} failed: {e}"));
    }
    error
}

fn run_one_cycle(
    db: &Database,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    state: &Mutex<WorkerState>,
) {
    let mut span = crate::telemetry::maybe_span("sync.cycle");
    let conn = db.conn();
    let mut error = sync_with_remote(&conn, hub_url, secret, node_id, HUB_REMOTE_ID);

    // Every discovered peer (static list plus Tailscale) gets the same
    // treatment as the hub: probed first (the only "is this really a
    // remind_me instance" check, matching the reference), skipped
    // silently if unreachable or if it names this very node.
    for peer in super::discover_peers() {
        if peer.node_id == node_id {
            continue;
        }
        if !super::probe_peer(&peer.url, secret) {
            continue;
        }
        if let Some(e) = sync_with_remote(&conn, &peer.url, secret, node_id, &peer.node_id) {
            error.get_or_insert(e);
        }
    }

    if let Err(e) = prune_outbox(&conn) {
        error.get_or_insert_with(|| format!("outbox prune failed: {e}"));
    }
    drop(conn);

    if error.is_some() {
        span.mark_error();
    }

    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    guard.cycles += 1;
    guard.last_cycle_at = Some(chrono::Utc::now().to_rfc3339());
    guard.last_error = error;
}

/// What `remind_me_server_status` reports for the background sync cycle.
///
/// # Why there are two error fields
///
/// `last_error` is **in-process state**: it is set at the end of every cycle
/// and cleared by that same process's next clean cycle. Nothing clears it
/// across processes, and the normal deployment runs one MCP server process per
/// connected client, all syncing the same database. So a process whose cycle
/// failed while the hub was unreachable would keep reporting that error
/// indefinitely, even after a sibling process retried successfully — while the
/// same report showed the `sync_log` watermarks advancing normally.
///
/// The two facts come from different places: this error lives in one process's
/// memory, the watermarks live in the shared `sync_log` table. [`superseded`]
/// compares them, and an error the watermarks have moved past is reported as
/// `superseded_error` instead. It is not discarded — the evidence of what
/// actually happened is worth keeping, the same reason `sync_repair` resets
/// only the cursors — but the field a reader reaches for first now answers
/// "is sync failing right now" correctly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncWorkerStatus {
    pub enabled: bool,
    pub running: bool,
    pub cycles: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cycle_at: Option<String>,
    /// The current failure, or `None` when sync is healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// A failure that a later successful cycle — possibly in another process —
    /// has already moved past. History, not current state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_error: Option<String>,
}

/// The disabled state — no `node_id`/`hub_url`/`secret` configured, or not
/// all three. Not an error: this is the ordinary case.
pub fn disabled_status() -> SyncWorkerStatus {
    SyncWorkerStatus {
        enabled: false,
        running: false,
        cycles: 0,
        last_cycle_at: None,
        last_error: None,
        superseded_error: None,
    }
}

/// Whether every remote has succeeded since the cycle at `failed_cycle_at`.
///
/// The `sync_log` watermarks only ever advance on success, so a remote whose
/// newest success is later than the failed cycle has been retried — possibly
/// by another process sharing this database — and that failure is no longer
/// the current state.
///
/// **Every** remote must have moved, not just one. The error is cycle-level
/// (the first failure across all remotes), so the remote that produced it is
/// not identifiable from the message; requiring all of them is the reading
/// that cannot hide a remote which is still stuck.
///
/// Returns `false` — report the error — in every case where the evidence does
/// not positively establish recovery:
///
/// - a missing or unparseable cycle timestamp, because a stamp this crate
///   cannot read is not grounds for calling sync healthy;
/// - no remotes at all, because there is nothing to supersede it;
/// - a remote still sitting at the epoch default, which means *never
///   succeeded* rather than *succeeded a long time ago*.
pub fn superseded(conn: &Connection, failed_cycle_at: Option<&str>) -> rusqlite::Result<bool> {
    let Some(failed_at) = failed_cycle_at.and_then(parse_ts) else {
        return Ok(false);
    };

    let mut stmt = conn.prepare("SELECT last_push_at, last_pull_at FROM sync_log")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if rows.is_empty() {
        return Ok(false);
    }

    for (push, pull) in rows {
        let newest = [push, pull]
            .iter()
            .filter(|at| at.as_str() != EPOCH)
            .filter_map(|at| parse_ts(at))
            .max();
        match newest {
            Some(at) if at > failed_at => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// A never-contacted remote sits here rather than at NULL, so "never" has to
/// be recognised by value or it reads as a very stale success.
const EPOCH: &str = "1970-01-01T00:00:00+00:00";

fn parse_ts(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}
