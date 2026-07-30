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

    pub fn status(&self) -> SyncWorkerStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        SyncWorkerStatus {
            enabled: true,
            running: self.is_running(),
            cycles: state.cycles,
            last_cycle_at: state.last_cycle_at.clone(),
            last_error: state.last_error.clone(),
        }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncWorkerStatus {
    pub enabled: bool,
    pub running: bool,
    pub cycles: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cycle_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
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
    }
}
