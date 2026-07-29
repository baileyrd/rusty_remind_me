//! Watched-folder ingestion: files appear in a directory, memories appear in
//! the store.
//!
//! # Polling, deliberately
//!
//! Directories are scanned on an interval rather than watched through
//! filesystem events. That is a choice, not a limitation: it needs no
//! additional dependency, it behaves identically on every platform, and it
//! matches the background-loop shape the rest of the system uses. An
//! event-based watcher would be a defensible alternative; it would also be a
//! new dependency and a new class of platform-specific failure.
//!
//! # The debounce is the subtle part
//!
//! A file whose modification time is younger than [`DEFAULT_GRACE_SECONDS`] is
//! **deferred** until a later scan observes the same `(mtime, size)`
//! signature. That is what stops a file being ingested while it is still being
//! written — a half-flushed export would otherwise become a truncated memory
//! that dedup then pins in place, because its hash is stable and wrong.
//!
//! A file modified *before* the grace window — the ordinary startup-backlog
//! case — ingests immediately. Only implementing the delay would give you a
//! watcher that waits a minute before touching anything on every restart.
//!
//! # Changed files supersede their previous import
//!
//! Re-importing an edited file leaves the previous version's chunks in the
//! store, where they would keep matching searches. Those memories are marked
//! superseded, which every read path already excludes. A memory the user
//! explicitly deleted is left alone — a re-import must not resurrect or
//! silently alter something someone chose to remove.

use crate::import_paths::{import_roots, is_contained, resolve_lexically, SUPPORTED_SUFFIXES};
use crate::importer::import_file;
use crate::models::{ImportKind, ImportOutcome};
use chrono::Utc;
use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Colon-separated directories to watch.
pub const WATCH_DIRS_ENV: &str = "REMIND_ME_WATCH_DIRS";
/// Seconds between scans.
pub const WATCH_INTERVAL_ENV: &str = "REMIND_ME_WATCH_INTERVAL";
/// Seconds a file must be untouched before it is considered stable.
pub const WATCH_GRACE_ENV: &str = "REMIND_ME_WATCH_GRACE";

pub const DEFAULT_INTERVAL_SECONDS: u64 = 60;
pub const DEFAULT_GRACE_SECONDS: u64 = 5;

/// Recent errors kept for the status report.
const ERROR_HISTORY: usize = 10;

/// What one scan pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCounts {
    /// Files imported this pass.
    pub ingested: usize,
    /// Files the importer recognised as already-imported content.
    pub skipped: usize,
    /// Files too fresh to trust yet; they will be reconsidered next pass.
    pub debounced: usize,
    /// Memories superseded because their file changed.
    pub superseded: usize,
    pub errors: usize,
}

/// A watch directory that was refused, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedDir {
    pub path: String,
    pub reason: String,
}

/// The watcher's state, for `remind_me_watch_status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchStatus {
    /// Whether any watch directory is configured at all.
    pub enabled: bool,
    pub watch_dirs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_dirs: Vec<RejectedDir>,
    pub interval_seconds: u64,
    pub grace_seconds: u64,
    pub scans: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scan_at: Option<String>,
    pub files_ingested: usize,
    pub files_skipped: usize,
    pub memories_superseded: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_errors: Vec<String>,
    /// Says what to configure when nothing is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

fn env_seconds(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Configured watch directories, split into accepted and refused.
///
/// A directory outside the import roots is **refused** — the watcher ingests
/// through the same importer as `remind_me_import_chat`, so it inherits the
/// same containment rule (`SE-02`). Letting a watch directory sit outside the
/// roots would be a way to import from anywhere by configuration, which is the
/// boundary the roots exist to draw.
///
/// A directory that does not exist yet is **accepted**: someone may create it
/// later, and scans skip what is not there.
pub fn validate_watch_dirs(dirs: &[PathBuf]) -> (Vec<PathBuf>, Vec<RejectedDir>) {
    let roots = import_roots();
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();

    for dir in dirs {
        let resolved = resolve_lexically(dir);
        if is_contained(&resolved, &roots) {
            accepted.push(resolved);
        } else {
            rejected.push(RejectedDir {
                path: resolved.display().to_string(),
                reason: "watch dir not in allowed import roots".to_string(),
            });
        }
    }
    (accepted, rejected)
}

/// Watch directories from the environment.
pub fn configured_watch_dirs() -> Vec<PathBuf> {
    std::env::var(WATCH_DIRS_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|raw| {
            raw.split(':')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Mark a previous import's memories superseded by a newer one.
///
/// Returns how many were superseded.
///
/// **A soft-deleted memory is left alone.** Re-importing a changed file must
/// not touch something the user explicitly removed — superseding it would be a
/// silent write to a record they had already decided about.
///
/// The value written is an *import* id rather than a memory id. Both satisfy
/// the `superseded_by IS NULL` filter every read path uses, so stale chunks
/// drop out of search while staying in the database for audit.
pub fn supersede_import(
    conn: &Connection,
    old_import_id: &str,
    new_import_id: &str,
) -> Result<usize> {
    let affected = conn.execute(
        "UPDATE memories
            SET superseded_by = ?, updated_at = ?
          WHERE superseded_by IS NULL
            AND deleted_at IS NULL
            AND json_extract(metadata, '$.import_id') = ?",
        params![new_import_id, Utc::now().to_rfc3339(), old_import_id],
    )?;
    Ok(affected)
}

/// A file's identity for change detection.
type Signature = (u64, u64);

/// Polls watch directories and ingests files that have settled.
///
/// One scan pass is [`Watcher::scan_once`], which is where all the behaviour
/// lives — the interval loop only calls it. Keeping the pass separately
/// callable is what makes the debounce testable without waiting on wall-clock
/// time.
pub struct Watcher {
    watch_dirs: Vec<PathBuf>,
    rejected: Vec<RejectedDir>,
    interval: u64,
    grace: u64,
    category: String,
    tags: Vec<String>,
    extract_mode: String,
    max_length: usize,

    /// Signature last *attempted*, so an unchanged file is not retried.
    attempted: HashMap<PathBuf, Signature>,
    /// Signature seen but not yet trusted, pending a second identical sighting.
    pending: HashMap<PathBuf, Signature>,
    /// The import each path most recently produced, for supersession.
    imports: HashMap<PathBuf, String>,

    scans: usize,
    last_scan_at: Option<String>,
    files_ingested: usize,
    files_skipped: usize,
    memories_superseded: usize,
    errors: std::collections::VecDeque<String>,
}

impl Watcher {
    /// Build a watcher over already-validated directories.
    pub fn new(watch_dirs: Vec<PathBuf>, rejected: Vec<RejectedDir>) -> Self {
        Self {
            watch_dirs,
            rejected,
            interval: env_seconds(WATCH_INTERVAL_ENV, DEFAULT_INTERVAL_SECONDS),
            grace: env_seconds(WATCH_GRACE_ENV, DEFAULT_GRACE_SECONDS),
            category: "chat_import".to_string(),
            tags: Vec::new(),
            extract_mode: "assistant_messages".to_string(),
            max_length: 10_000,
            attempted: HashMap::new(),
            pending: HashMap::new(),
            imports: HashMap::new(),
            scans: 0,
            last_scan_at: None,
            files_ingested: 0,
            files_skipped: 0,
            memories_superseded: 0,
            errors: std::collections::VecDeque::new(),
        }
    }

    /// Build a watcher from the environment, or `None` when nothing is
    /// configured or every configured directory was refused.
    ///
    /// Gated on configuration for the same reason the webhook endpoint is: a
    /// watcher with no directories is not a running feature, and reporting it
    /// as one would be misleading.
    pub fn from_env() -> Option<Self> {
        let configured = configured_watch_dirs();
        if configured.is_empty() {
            return None;
        }
        let (accepted, rejected) = validate_watch_dirs(&configured);
        if accepted.is_empty() {
            return None;
        }
        Some(Self::new(accepted, rejected))
    }

    /// Seconds between scans.
    pub fn interval(&self) -> u64 {
        self.interval
    }

    pub fn with_grace(mut self, seconds: u64) -> Self {
        self.grace = seconds;
        self
    }

    /// Supported, non-hidden files under every watch directory.
    ///
    /// Hidden entries are judged **relative to the watch directory**, so
    /// watching `~/.notes` works while `~/notes/.git/` is still skipped.
    fn candidates(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for root in &self.watch_dirs {
            if !root.is_dir() {
                // May be created later; scans skip what is not there.
                continue;
            }
            collect(root, root, &mut files);
        }
        files.sort();
        files.dedup();
        files
    }

    /// Run one scan pass.
    pub fn scan_once(&mut self, conn: &Connection) -> ScanCounts {
        let mut counts = ScanCounts::default();
        let now = now_seconds();
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        for path in self.candidates() {
            let Some(signature) = signature_of(&path) else {
                continue; // vanished mid-scan
            };
            seen.insert(path.clone());

            if self.attempted.get(&path) == Some(&signature) {
                // Unchanged since the last attempt: not work, and not counted.
                continue;
            }
            let age = now.saturating_sub(signature.0);
            if age < self.grace && self.pending.get(&path) != Some(&signature) {
                // Too fresh, and this is the first sighting at this signature.
                self.pending.insert(path.clone(), signature);
                counts.debounced += 1;
                continue;
            }

            self.ingest(conn, &path, signature, &mut counts);
            self.pending.remove(&path);
        }

        // Forget per-path state for files that vanished, so a recreated file
        // debounces again. `imports` is kept deliberately: a file that comes
        // back changed still has to supersede its previous import's memories.
        self.pending.retain(|path, _| seen.contains(path));
        self.attempted.retain(|path, _| seen.contains(path));

        self.scans += 1;
        self.last_scan_at = Some(Utc::now().to_rfc3339());
        self.files_ingested += counts.ingested;
        self.files_skipped += counts.skipped;
        self.memories_superseded += counts.superseded;
        counts
    }

    fn ingest(
        &mut self,
        conn: &Connection,
        path: &Path,
        signature: Signature,
        counts: &mut ScanCounts,
    ) {
        let outcome = import_file(
            conn,
            path,
            &self.category,
            &self.tags,
            &self.extract_mode,
            self.max_length,
            ImportKind::Auto,
        );

        // Record the signature whatever happened, including on failure: a file
        // that cannot be parsed should not be retried every minute until
        // someone changes it.
        self.attempted.insert(path.to_path_buf(), signature);

        match outcome {
            Ok(ImportOutcome::Imported { import_id, .. }) => {
                let superseded = match self.imports.get(path) {
                    Some(previous) if *previous != import_id => {
                        supersede_import(conn, previous, &import_id).unwrap_or(0)
                    }
                    _ => 0,
                };
                self.imports.insert(path.to_path_buf(), import_id);
                counts.ingested += 1;
                counts.superseded += superseded;
            }
            Ok(ImportOutcome::Skipped { import_id, .. }) => {
                // Adopt the existing import so a later edit supersedes it —
                // this is the first-scan-after-restart case.
                self.imports.insert(path.to_path_buf(), import_id);
                counts.skipped += 1;
            }
            Ok(ImportOutcome::Failed { reason, .. }) => {
                counts.errors += 1;
                self.record_error(format!("{}: {}", path.display(), reason));
            }
            Err(e) => {
                counts.errors += 1;
                self.record_error(format!("{}: {}", path.display(), e));
            }
        }
    }

    fn record_error(&mut self, message: String) {
        if self.errors.len() >= ERROR_HISTORY {
            self.errors.pop_front();
        }
        self.errors.push_back(message);
    }

    /// A status snapshot.
    pub fn status(&self) -> WatchStatus {
        WatchStatus {
            enabled: true,
            watch_dirs: self
                .watch_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect(),
            rejected_dirs: self.rejected.clone(),
            interval_seconds: self.interval,
            grace_seconds: self.grace,
            scans: self.scans,
            last_scan_at: self.last_scan_at.clone(),
            files_ingested: self.files_ingested,
            files_skipped: self.files_skipped,
            memories_superseded: self.memories_superseded,
            recent_errors: self.errors.iter().cloned().collect(),
            hint: None,
        }
    }
}

/// The status of a watcher that is not configured.
///
/// Distinguishable from a configured-but-idle one: it says what to set rather
/// than reporting a bare `false`, which could not tell "no watcher here" apart
/// from "the watcher stopped".
pub fn disabled_status() -> WatchStatus {
    let configured = configured_watch_dirs();
    let (_, rejected) = validate_watch_dirs(&configured);
    WatchStatus {
        enabled: false,
        watch_dirs: Vec::new(),
        rejected_dirs: rejected,
        interval_seconds: env_seconds(WATCH_INTERVAL_ENV, DEFAULT_INTERVAL_SECONDS),
        grace_seconds: env_seconds(WATCH_GRACE_ENV, DEFAULT_GRACE_SECONDS),
        scans: 0,
        last_scan_at: None,
        files_ingested: 0,
        files_skipped: 0,
        memories_superseded: 0,
        recent_errors: Vec::new(),
        hint: Some(format!(
            "set {} to colon-separated directories inside the import roots to \
             enable the folder watcher",
            WATCH_DIRS_ENV
        )),
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Hidden relative to the watch root, so watching a hidden directory
        // still works while `.git` inside it does not.
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
            && path != root
        {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, out);
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .is_some_and(|s| SUPPORTED_SUFFIXES.contains(&s.as_str()))
        {
            out.push(path);
        }
    }
}

fn signature_of(path: &Path) -> Option<Signature> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, metadata.len()))
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
