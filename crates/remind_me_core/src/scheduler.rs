//! The background loop that delivers a due reminder, exactly once.
//!
//! # Why a deliveries table and not "`remind_at` is in the past"
//!
//! A bare past-timestamp check has no way to remember that it already fired,
//! so it leaves only two behaviours, both wrong: deliver on every pass forever,
//! or skip anything already past and lose it. Recording each delivery in
//! `reminder_deliveries` — keyed `(memory_id, remind_at)`, uniquely indexed —
//! is what makes the third behaviour possible, which is the one people expect:
//! a reminder that came due while nothing was running is delivered once on the
//! next pass after start, and then never again.
//!
//! That is confirmed against the reference rather than assumed: its `poll_once`
//! selects `remind_at <= now` with no lower bound, so it delivers late rather
//! than skipping.
//!
//! # A failed channel does not hold the reminder back
//!
//! Delivery is a log line *plus* a best-effort fan-out to whatever
//! notification channels are configured. The delivery row is written either
//! way, so a webhook that is down does not cause the same reminder to be
//! re-attempted on the next pass. The reference makes the same call, and the
//! reason holds up: the log line is the channel that cannot fail, so the
//! reminder has been delivered somewhere. Retrying instead would mean
//! re-logging the same reminder every 60 seconds for as long as the webhook
//! stayed down — turning one missed notification into an unbounded stream of
//! duplicates through the channel that *was* working.
//!
//! # Nothing configured still delivers
//!
//! With no channel set up, [`poll_once`] still logs and still records the
//! delivery. "Channels are opt-in" governs whether a *notification* is
//! attempted, not whether the reminder is considered handled — a vault with no
//! webhook must not silently accumulate undelivered reminders forever.

use crate::models::Memory;
use crate::notifications;
use crate::reminders;
use rusqlite::{params, Connection, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const POLL_INTERVAL_ENV: &str = "REMIND_ME_REMINDER_POLL_INTERVAL";

/// Seconds between passes. The scheduler always runs; this only tunes how
/// often it looks, not whether it is enabled.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 60;

/// How much of a memory's content a delivery line carries. Enough to
/// recognise which reminder fired without pasting a whole document into a log.
const PREVIEW_CHARS: usize = 200;

pub fn configured_poll_interval() -> Duration {
    Duration::from_secs(
        std::env::var(POLL_INTERVAL_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
    )
}

fn preview(content: &str) -> String {
    let mut out: String = content.chars().take(PREVIEW_CHARS).collect();
    if content.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// Reminders that are due and not yet delivered.
///
/// This is `reminders::list_reminders`' `Overdue` window with no limit, and
/// deliberately the same definition: the scheduler deciding "due" differently
/// from what `remind_me_list_reminders` shows would mean a reminder could sit
/// visibly overdue while the loop never picked it up.
pub fn due_reminders(conn: &Connection) -> Result<Vec<Memory>> {
    reminders::list_reminders(conn, crate::models::ReminderWindow::Overdue, i64::MAX)
}

/// Log the reminder, then fan out to any configured channel.
///
/// Logging is unconditional and happens first, so it survives a channel that
/// is misconfigured, slow, or absent.
pub fn deliver(memory: &Memory) {
    eprintln!(
        "reminder due — memory `{}` (remind_at={}): {}",
        memory.id,
        memory.remind_at.as_deref().unwrap_or("?"),
        preview(&memory.content)
    );
    notifications::notify(
        &format!("Reminder due: memory `{}`", memory.id),
        &memory.content,
    );
}

/// Deliver every due, not-yet-delivered reminder once. Returns how many.
pub fn poll_once(conn: &Connection) -> Result<usize> {
    poll_once_with(conn, &mut deliver)
}

/// [`poll_once`] with the delivery step injected.
///
/// The seam exists so a test can assert *which* reminders a pass picks up
/// without a live webhook, and so an embedder can replace delivery wholesale
/// without touching the due query.
pub fn poll_once_with(conn: &Connection, deliver: &mut dyn FnMut(&Memory)) -> Result<usize> {
    let due = due_reminders(conn)?;
    let mut delivered = 0usize;

    for memory in &due {
        let Some(remind_at) = memory.remind_at.as_deref() else {
            continue;
        };
        deliver(memory);
        // Written after the delivery attempt, not before: a panic in a
        // delivery hook should leave the reminder pending rather than mark it
        // handled. `INSERT OR IGNORE` because the unique index is the real
        // guarantee — two racing pollers must produce one delivery, not an
        // error that aborts the pass.
        conn.execute(
            "INSERT OR IGNORE INTO reminder_deliveries (memory_id, remind_at, delivered_at)
             VALUES (?, ?, ?)",
            params![memory.id, remind_at, chrono::Utc::now().to_rfc3339()],
        )?;
        delivered += 1;
    }

    Ok(delivered)
}

// ---------------------------------------------------------------------------
// Thread lifecycle
// ---------------------------------------------------------------------------

/// Shared stop flag plus the condvar the loop sleeps on.
///
/// A condvar rather than `thread::sleep`: shutdown has to interrupt the wait,
/// or stopping the server would block for up to a full poll interval on a
/// thread that has nothing left to do.
struct Stop {
    stopped: AtomicBool,
    waker: Mutex<()>,
    condvar: Condvar,
}

impl Stop {
    fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            waker: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _guard = self.waker.lock().unwrap();
        self.condvar.notify_all();
    }

    /// Sleep for `interval`, waking early if stopped.
    fn wait(&self, interval: Duration) {
        let guard = self.waker.lock().unwrap();
        if self.is_stopped() {
            return;
        }
        let _unused = self.condvar.wait_timeout(guard, interval).unwrap();
    }
}

/// A running scheduler. Dropping it does **not** stop the loop — call
/// [`SchedulerHandle::stop`], which joins, so an in-flight pass cannot still
/// be writing while the caller tears the database down underneath it.
pub struct SchedulerHandle {
    stop: Arc<Stop>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SchedulerHandle {
    pub fn stop(mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start the polling loop against a database at `db_path`.
///
/// The thread opens its own connection rather than sharing the caller's:
/// `rusqlite::Connection` is not `Sync`, and passing one across would trade a
/// compile error for a runtime serialisation problem.
///
/// Unconditional, unlike the folder watcher — reminders have no enable switch,
/// only an interval.
/// Where this connection's database lives, or `None` for an in-memory one.
///
/// Following the shape `pid`/`backup`/`status` already established — each
/// keeps its own copy of this one-line `PRAGMA` rather than sharing a helper.
fn database_path(conn: &Connection) -> Option<std::path::PathBuf> {
    let path: String = conn
        .query_row("PRAGMA database_list", [], |row| row.get(2))
        .ok()?;
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}

/// Start the scheduler for the database `conn` is attached to.
///
/// Returns `None` for an in-memory database: the loop's thread opens its own
/// connection by path, and `:memory:` would give it a *different*, empty
/// database rather than this one. Silently polling an empty database forever
/// would look exactly like a vault with nothing due.
pub fn start_scheduler_for(conn: &Connection) -> Option<SchedulerHandle> {
    Some(start_scheduler(database_path(conn)?))
}

pub fn start_scheduler(db_path: std::path::PathBuf) -> SchedulerHandle {
    let stop = Arc::new(Stop::new());
    let loop_stop = Arc::clone(&stop);
    let interval = configured_poll_interval();

    let thread = std::thread::Builder::new()
        .name("reminder-scheduler".to_string())
        .spawn(move || {
            let conn = match Connection::open(&db_path) {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("reminder scheduler: cannot open {:?}: {}", db_path, e);
                    return;
                }
            };
            while !loop_stop.is_stopped() {
                // A failed pass is reported and the loop continues. A
                // transient database error must not silently end reminder
                // delivery for the rest of the process's life.
                match poll_once(&conn) {
                    Ok(0) => {}
                    Ok(n) => eprintln!("reminder scheduler: delivered {} reminder(s)", n),
                    Err(e) => eprintln!("reminder scheduler: poll failed: {}", e),
                }
                loop_stop.wait(interval);
            }
        })
        .expect("spawning the reminder scheduler thread");

    SchedulerHandle {
        stop,
        thread: Some(thread),
    }
}
