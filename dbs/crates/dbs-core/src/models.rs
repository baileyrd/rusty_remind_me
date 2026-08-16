//! Core data models exchanged between connectors and the engine.
//!
//! Mirrors `src/dbs/core/models.py` in baileyrd/Daily-Backup-System (pinned
//! `@6cc6491`). Two families live here, same split as the reference:
//!
//! * **Connector-facing** (the plugin contract): [`MediaRef`], [`BackupItem`],
//!   [`Cursor`], [`Checkpoint`], [`ReconcileMarker`], [`FetchEvent`].
//! * **Engine/service results** (plain, JSON-serializable data): [`RunResult`],
//!   [`RunStatus`], [`SourceStatus`], [`ConnectorInfo`], and friends.
//!
//! `RunContext` (the reference's per-run injected context, carrying
//! `Secrets`/`ManagedHTTPClient`/`CancelToken`) is deliberately **not**
//! implemented here — those dependent types don't exist yet (separate
//! issues). It belongs with the connector trait, not the plain data model.
//!
//! Design rules carried over from the reference:
//!
//! * [`BackupItem::raw`] is the verbatim upstream payload — the source of
//!   truth — and is never coerced or reshaped.
//! * [`Cursor`] is opaque to the engine; connectors own its shape. The
//!   engine persists it verbatim and only ever hands it back.
//! * A connector never writes the cursor directly; it yields a
//!   [`Checkpoint`], and the engine commits buffered items + the new cursor
//!   in a single transaction — what makes partial-failure forward progress
//!   safe.

use std::collections::HashSet;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capabilities::{AuthCapture, Capabilities, ItemKind};

// --------------------------------------------------------------------- //
// Connector-facing models                                               //
// --------------------------------------------------------------------- //

/// A referenced media asset attached to an item (e.g. a thumbnail/cover).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRef {
    pub url: String,
    /// Informal, not enforced: `"image"` | `"video"` | `"file"` | `"archive"`.
    #[serde(default = "MediaRef::default_kind")]
    pub kind: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    /// Optional connector-prefetched bytes. When set, storage persists these
    /// directly instead of resolving `url`; `url` stays the reference of
    /// record either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<u8>>,
}

impl MediaRef {
    fn default_kind() -> String {
        "image".to_string()
    }

    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: Self::default_kind(),
            filename: None,
            mime: None,
            data: None,
        }
    }
}

/// A non-empty `external_id` was required but not provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyExternalId;

impl fmt::Display for EmptyExternalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("external_id must be a non-empty string")
    }
}

impl std::error::Error for EmptyExternalId {}

/// A single record yielded by a connector.
///
/// `raw` is the verbatim upstream object. The normalized fields
/// (`title`/`url`/`body`/`tags`/...) are best-effort projections used for
/// querying and export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupItem {
    external_id: String,
    pub item_kind: String,
    pub raw: Value,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub media: Vec<MediaRef>,
    /// Optional connector-supplied change token (etag/version). When set,
    /// the engine uses it for change detection instead of hashing the
    /// projection.
    #[serde(default)]
    pub revision_token: Option<String>,
}

impl BackupItem {
    /// Constructs a `BackupItem`, rejecting an empty `external_id` the same
    /// way the reference's `field_validator` does.
    pub fn new(
        external_id: impl Into<String>,
        item_kind: impl Into<String>,
        raw: Value,
    ) -> Result<Self, EmptyExternalId> {
        let external_id = external_id.into();
        if external_id.trim().is_empty() {
            return Err(EmptyExternalId);
        }
        Ok(Self {
            external_id,
            item_kind: item_kind.into(),
            raw,
            title: None,
            url: None,
            body: None,
            tags: Vec::new(),
            created_at: None,
            updated_at: None,
            deleted: false,
            media: Vec::new(),
            revision_token: None,
        })
    }

    pub fn external_id(&self) -> &str {
        &self.external_id
    }
}

/// An opaque, connector-owned incremental position.
///
/// The engine persists `value` verbatim as JSON and never interprets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub value: Value,
}

/// Yielded between items to mark a safe commit point.
///
/// When the engine sees a checkpoint it flushes all buffered items *and*
/// persists `cursor` in one transaction, so the stored cursor can never run
/// ahead of durable data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub cursor: Cursor,
    #[serde(default)]
    pub note: String,
}

/// Yielded during a full enumeration to enable deletion detection.
///
/// After a *successful* full/reconcile run the engine soft-deletes any
/// non-deleted item whose `external_id` is absent from `live_ids`. Honored
/// only when the connector declares `supports_full_enumeration`.
///
/// `scope` bounds the sweep's candidate set: `"source"` (default) means
/// every live item of the source is a candidate; `"tag:<value>"` restricts
/// it to live items carrying that tag (for connectors whose enumeration is
/// complete per-partition but not overall).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconcileMarker {
    pub live_ids: HashSet<String>,
    #[serde(default = "ReconcileMarker::default_scope")]
    pub scope: String,
}

impl ReconcileMarker {
    fn default_scope() -> String {
        "source".to_string()
    }

    pub fn new(live_ids: HashSet<String>) -> Self {
        Self {
            live_ids,
            scope: Self::default_scope(),
        }
    }
}

/// The unified yield type of a connector's fetch stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FetchEvent {
    Item(BackupItem),
    Checkpoint(Checkpoint),
    ReconcileMarker(ReconcileMarker),
}

// --------------------------------------------------------------------- //
// Engine/service result models (plain, render-free)                     //
// --------------------------------------------------------------------- //

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Success,
    Partial,
    /// Default: a [`ConnectorRunOutcome`](crate::service::ConnectorRunOutcome)
    /// that hasn't been filled in yet (e.g. the `unwrap_or_else` fallback
    /// in `BackupService::backup_source` when a `ConnectorRunner` errors)
    /// should read as failed, not silently succeeded.
    #[default]
    Failed,
    Skipped,
    Interrupted,
}

/// Outcome of one source backup. Plain data; no rendering, JSON-friendly
/// via `#[derive(Serialize)]` (the reference's `to_dict` is redundant here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub source: String,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    #[serde(default = "RunResult::default_mode")]
    pub mode: String,
    #[serde(default)]
    pub run_id: Option<i64>,
    #[serde(default)]
    pub fetched: u64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub unchanged: u64,
    #[serde(default)]
    pub deleted: u64,
    #[serde(default)]
    pub undeleted: u64,
    #[serde(default)]
    pub revisions: u64,
    /// Connector-reported soft failures (e.g. media that failed and will
    /// retry).
    #[serde(default)]
    pub items_failed: u64,
    #[serde(default)]
    pub error: Option<String>,
    /// "Succeeded with caveats" — kept separate from `error` so a `Success`
    /// run's caveats are visible without masquerading as a failure.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl RunResult {
    fn default_mode() -> String {
        "incremental".to_string()
    }

    /// Wall-clock milliseconds from start to finish, floored at zero.
    pub fn duration_ms(&self) -> i64 {
        (self.finished_at - self.started_at)
            .num_milliseconds()
            .max(0)
    }

    /// A zero-activity `Skipped` result at a single instant — used for
    /// the early-exit paths in `service::BackupService::backup_source`
    /// (disabled source, VPN guard, dry-run).
    pub fn skipped(
        source: impl Into<String>,
        at: DateTime<Utc>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            status: RunStatus::Skipped,
            started_at: at,
            finished_at: at,
            mode: Self::default_mode(),
            run_id: None,
            fetched: 0,
            created: 0,
            updated: 0,
            unchanged: 0,
            deleted: 0,
            undeleted: 0,
            revisions: 0,
            items_failed: 0,
            error: Some(reason.into()),
            warnings: Vec::new(),
        }
    }

    /// A zero-activity `Failed` result at a single instant — used when a
    /// source-level error (not a connector-fetch error) aborts a run
    /// before it can begin, e.g. in `BackupService::backup_all`'s
    /// `continue_on_error` path.
    pub fn failed(source: impl Into<String>, at: DateTime<Utc>, reason: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed,
            ..Self::skipped(source, at, reason)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    SourceStart,
    Item,
    Checkpoint,
    Sweep,
    SourceDone,
}

/// A point-in-time progress update for one source's backup run.
///
/// Item *totals* are generally unknown up front, so `fetched` is a running
/// count, not a fraction. `source_index`/`source_total` give determinate
/// cross-source progress for `dbs backup --all`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub phase: ProgressPhase,
    pub source: String,
    pub mode: String,
    #[serde(default)]
    pub fetched: u64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub unchanged: u64,
    #[serde(default)]
    pub deleted: u64,
    #[serde(default)]
    pub source_index: Option<u32>,
    #[serde(default)]
    pub source_total: Option<u32>,
    /// Set on `SourceDone`.
    #[serde(default)]
    pub result: Option<RunResult>,
    #[serde(default)]
    pub note: String,
}

/// Snapshot of one source for `dbs status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub enabled: bool,
    pub total_items: u64,
    pub live_items: u64,
    pub deleted_items: u64,
    pub last_run_status: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_mode: Option<String>,
    pub run_count: u64,
    pub watermark: Option<DateTime<Utc>>,
    pub has_interrupted_runs: bool,
    #[serde(default = "SourceStatus::default_schedule")]
    pub schedule: String,
    /// `None` means due right now.
    #[serde(default)]
    pub next_due_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub due_now: bool,
    /// Mirrors `SourceConfig::requires_vpn` (issue #170) — the web UI's
    /// `/api/status` needs this per row to tag VPN-gated sources and
    /// disable their Run button when the tunnel is down, without a
    /// second round-trip to re-read `Config` for something `status()`
    /// already has in hand while building this row.
    #[serde(default)]
    pub requires_vpn: bool,
}

impl SourceStatus {
    fn default_schedule() -> String {
        "daily".to_string()
    }
}

/// Describes a discovered connector for `dbs connectors`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorInfo {
    #[serde(rename = "type")]
    pub type_: String,
    pub plugin_id: String,
    pub dist_name: String,
    pub is_builtin: bool,
    pub display_name: String,
    pub description: String,
    pub capabilities: Capabilities,
    pub item_kinds: Vec<ItemKind>,
    pub secret_keys: Vec<String>,
    #[serde(default)]
    pub config_schema: Value,
    /// `None` for a connector with no interactive-login capture story
    /// (most of them — plain API-token auth). The web UI (issue #172)
    /// reads this to show a capture/import button at all, and its
    /// `per_source` field to decide what the button targets.
    #[serde(default)]
    pub auth_capture: Option<AuthCapture>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyIssue {
    pub source: String,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    pub ok: bool,
    #[serde(default)]
    pub issues: Vec<VerifyIssue>,
}

/// One `dbs doctor` finding. `status` is `"ok"` / `"warn"` / `"fail"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: String,
    pub detail: String,
}

/// Result of a database maintenance pass. Plain data; no rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceReport {
    pub database: String,
    pub wal_checkpointed: bool,
    pub optimized: bool,
    pub vacuumed: bool,
    pub size_before: u64,
    pub size_after: u64,
    #[serde(default)]
    pub snapshot_path: Option<String>,
    #[serde(default)]
    pub snapshot_bytes: Option<u64>,
    #[serde(default)]
    pub revisions_pruned: u64,
}

/// Result of replaying an export back into the database. Plain data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub path: String,
    pub dry_run: bool,
    pub sources: Vec<String>,
    #[serde(default)]
    pub fetched: u64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub unchanged: u64,
    #[serde(default)]
    pub deleted: u64,
    #[serde(default)]
    pub revisions_skipped: u64,
    #[serde(default)]
    pub media_skipped: u64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backup_item_rejects_empty_external_id() {
        assert!(BackupItem::new("", "post", json!({})).is_err());
        assert!(BackupItem::new("   ", "post", json!({})).is_err());
    }

    #[test]
    fn backup_item_accepts_non_empty_external_id() {
        let item = BackupItem::new("abc123", "post", json!({"title": "hi"})).unwrap();
        assert_eq!(item.external_id(), "abc123");
        assert!(!item.deleted);
        assert!(item.tags.is_empty());
    }

    #[test]
    fn reconcile_marker_defaults_to_source_scope() {
        let marker = ReconcileMarker::new(HashSet::from(["a".to_string(), "b".to_string()]));
        assert_eq!(marker.scope, "source");
        assert_eq!(marker.live_ids.len(), 2);
    }

    #[test]
    fn run_result_duration_is_floored_at_zero() {
        let now = Utc::now();
        let result = RunResult {
            source: "raindrop".to_string(),
            status: RunStatus::Success,
            started_at: now,
            finished_at: now - chrono::Duration::seconds(5),
            mode: RunResult::default_mode(),
            run_id: None,
            fetched: 0,
            created: 0,
            updated: 0,
            unchanged: 0,
            deleted: 0,
            undeleted: 0,
            revisions: 0,
            items_failed: 0,
            error: None,
            warnings: Vec::new(),
        };
        // finished_at before started_at (clock skew) never yields negative duration.
        assert_eq!(result.duration_ms(), 0);
    }

    #[test]
    fn run_result_round_trips_through_json() {
        let now = Utc::now();
        let result = RunResult {
            source: "github".to_string(),
            status: RunStatus::Partial,
            started_at: now,
            finished_at: now,
            mode: "reconcile".to_string(),
            run_id: Some(42),
            fetched: 10,
            created: 3,
            updated: 2,
            unchanged: 5,
            deleted: 0,
            undeleted: 0,
            revisions: 5,
            items_failed: 1,
            error: None,
            warnings: vec!["zero-item run".to_string()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let round_tripped: RunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, round_tripped);
    }

    #[test]
    fn media_ref_default_kind_is_image() {
        let media = MediaRef::new("https://example.com/x.png");
        assert_eq!(media.kind, "image");
        assert!(media.data.is_none());
    }
}
