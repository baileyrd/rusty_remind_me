//! Sync-domain maintenance.
//!
//! This crate has no sync engine yet — see the epic tracking it — but the
//! generated schema carries `remind_me`'s outbox triggers, so the write side of
//! sync is already live here. This module holds the parts that have to exist
//! regardless of whether anything is draining the outbox.

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, Result};

/// Days an unsent outbox row is kept before being pruned.
///
/// Mirrors the reference's `OUTBOX_RETENTION_DAYS`, including the environment
/// variable that overrides it, so a database shared between the two systems is
/// governed by one policy rather than two.
pub const DEFAULT_OUTBOX_RETENTION_DAYS: i64 = 30;
const OUTBOX_RETENTION_ENV: &str = "REMIND_ME_OUTBOX_RETENTION_DAYS";

fn outbox_retention_days() -> i64 {
    std::env::var(OUTBOX_RETENTION_ENV)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d >= 0)
        .unwrap_or(DEFAULT_OUTBOX_RETENTION_DAYS)
}

/// Prune already-sent outbox rows and anything past the retention window.
///
/// Returns the number of rows removed.
///
/// # Why this runs at all
///
/// `memories_outbox_ai` and `memories_outbox_au` fire on every insert and every
/// update of `memories`, writing a full JSON snapshot of the row. Since
/// retrieval records access — which is an `UPDATE` — the outbox grows on reads
/// as well as writes. Nothing in this crate drains it, so without a prune it
/// grows without bound, carrying a copy of every memory's content each time.
///
/// # Why this policy and not another
///
/// This is the reference's own rule, verbatim: rows already marked sent are
/// echo-suppressed and never pushed, so they go immediately; the rest are kept
/// for the retention window so an intermittently-reachable remote can still
/// catch up, then dropped along with their per-remote send markers.
///
/// Copying the policy rather than inventing one matters because a database can
/// be shared with `remind_me` — it opens the same file and prunes on the same
/// rule, so anything this deletes is something the reference would have deleted
/// too. A tighter rule here would silently drop changes the reference still
/// intended to push.
///
/// # Where it runs
///
/// The reference prunes on each sync cycle. This crate has no sync cycle, so it
/// prunes on open. That bounds a long-lived database, but it does **not** bound
/// a single process that stays up longer than the retention window — whatever
/// implements sync should call this per cycle, which is the arrangement the
/// reference already has.
pub fn prune_outbox(conn: &Connection) -> Result<usize> {
    let cutoff = (Utc::now() - Duration::days(outbox_retention_days())).to_rfc3339();
    let removed = conn.execute(
        "DELETE FROM sync_outbox WHERE sent_at != '' OR created_at < ?",
        params![cutoff],
    )?;
    conn.execute(
        "DELETE FROM sync_sends WHERE outbox_id NOT IN (SELECT id FROM sync_outbox)",
        [],
    )?;
    Ok(removed)
}
