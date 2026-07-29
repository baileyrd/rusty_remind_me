//! Operational status: is this working, and where is the data.
//!
//! # Reporting absence honestly
//!
//! The reference reports on subsystems this crate does not have — a dashboard
//! UI, an embedding backend, sync. Rather than emit a field that always reads
//! "not running" as though it might one day read otherwise, each of those is
//! reported as [`SubsystemStatus::NotImplemented`] carrying the reason. A
//! caller can tell "this crate has no dashboard" apart from "the dashboard is
//! down", which a bare boolean cannot express.
//!
//! # No network
//!
//! Nothing here makes a network call. The reference's embedding probe may hit
//! the network and is flagged as a performance concern; a status tool that
//! hangs is worse than one that omits a field.

use crate::backup::{backup_dir, list_backups, BackupInfo};
use crate::db::migrations::SCHEMA_VERSION;
use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How a subsystem stands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubsystemStatus {
    Active,
    /// Present in `remind_me`, not built here yet. `reason` says so, so this
    /// is distinguishable from a subsystem that exists and is merely stopped.
    NotImplemented {
        reason: String,
    },
}

impl SubsystemStatus {
    fn missing(reason: &str) -> Self {
        Self::NotImplemented {
            reason: reason.to_string(),
        }
    }
}

/// A snapshot of where the data lives and what is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    /// `None` for an in-memory database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_path: Option<String>,
    pub database_exists: bool,
    pub database_bytes: Option<u64>,
    /// What `PRAGMA user_version` reports.
    pub schema_version: i32,
    /// What this build's generated schema corresponds to.
    pub expected_schema_version: i32,
    /// False when the two disagree — which is what makes a database
    /// unreadable to `remind_me`, so it is worth surfacing rather than
    /// inferring.
    pub schema_current: bool,
    pub memory_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<String>,
    pub backup_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_backup: Option<BackupInfo>,
    pub mcp: SubsystemStatus,
    pub dashboard: SubsystemStatus,
    pub embeddings: SubsystemStatus,
    pub sync: SubsystemStatus,
    /// Reported by the watcher itself now that one exists, rather than as an
    /// absent subsystem.
    pub watcher: crate::watcher::WatchStatus,
}

fn database_file(conn: &Connection) -> Result<Option<PathBuf>> {
    let path: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
    Ok(if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    })
}

/// Collect the status snapshot.
///
/// Reports only what this crate actually has. Subsystems it lacks are named
/// with a reason rather than reported as stopped — shipping a field that reads
/// "not running" for something that was never built is the hollow-stub failure
/// mode, and it makes a real outage indistinguishable from an absent feature.
pub fn server_status(conn: &Connection) -> Result<ServerStatus> {
    let database_path = database_file(conn)?;
    let database_exists = database_path.as_ref().map(|p| p.exists()).unwrap_or(true); // in-memory: it exists, it just has no file
    let database_bytes = database_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());

    let schema_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let memory_count: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
        [],
        |row| row.get(0),
    )?;

    // An in-memory database has no directory to hold backups, so this is
    // absent rather than empty.
    let dir = backup_dir(conn).ok();
    let backups = dir
        .as_ref()
        .and_then(|d| list_backups(d).ok())
        .unwrap_or_default();

    Ok(ServerStatus {
        database_path: database_path.map(|p| p.display().to_string()),
        database_exists,
        database_bytes,
        schema_version,
        expected_schema_version: SCHEMA_VERSION,
        schema_current: schema_version == SCHEMA_VERSION,
        memory_count,
        backup_dir: dir.map(|d| d.display().to_string()),
        backup_count: backups.len(),
        latest_backup: backups.into_iter().next(),
        mcp: SubsystemStatus::Active,
        dashboard: SubsystemStatus::missing("no dashboard UI in this crate"),
        embeddings: SubsystemStatus::missing(
            "no embedding backend in this crate; search is keyword-only",
        ),
        sync: SubsystemStatus::missing(
            "no sync engine in this crate; the outbox is written and pruned, never drained",
        ),
        watcher: match crate::watcher::Watcher::from_env() {
            Some(w) => w.status(),
            None => crate::watcher::disabled_status(),
        },
    })
}
