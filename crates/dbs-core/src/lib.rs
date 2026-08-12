//! Core data model and plugin contract for `rusty_dbs`.
//!
//! Mirrors `dbs.core` in baileyrd/Daily-Backup-System (pinned `@6cc6491`) —
//! see that repo's `docs/architecture.md` for the system this crate is
//! working toward parity with, and this repo's `gap-analysis.md` for the
//! full capability inventory and round-1 scope decisions.

pub mod cancel;
pub mod capabilities;
pub mod connector;
pub mod errors;
pub mod hashing;
pub mod models;
pub mod secrets;
pub mod storage;
pub mod timeutil;
pub mod versioning;

pub use cancel::CancelToken;
pub use capabilities::{AuthCapture, Capabilities, ItemKind};
pub use connector::{Connector, RunContext, CORE_API_VERSION};
pub use errors::{BackupRunError, ConnectorError, ConnectorLoadError, DbsError};
pub use hashing::{canonical_json, content_hash};
pub use models::{
    BackupItem, Checkpoint, ConnectorInfo, Cursor, DoctorCheck, FetchEvent, MaintenanceReport,
    MediaRef, ProgressEvent, ProgressPhase, ReconcileMarker, RestoreReport, RunResult, RunStatus,
    SourceStatus, VerifyIssue, VerifyReport,
};
pub use secrets::Secrets;
pub use storage::sqlite::open_connection;
pub use storage::{BatchResult, ExportQuery, ItemRow, PreparedItem, SourceRecord, Storage};
pub use timeutil::{iso_z, parse_iso};
pub use versioning::{is_api_compatible, CURRENT_API_VERSION};
