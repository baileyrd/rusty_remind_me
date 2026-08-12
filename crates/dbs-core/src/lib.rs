//! Core data model and plugin contract for `rusty_dbs`.
//!
//! Mirrors `dbs.core` in baileyrd/Daily-Backup-System (pinned `@6cc6491`) —
//! see that repo's `docs/architecture.md` for the system this crate is
//! working toward parity with, and this repo's `gap-analysis.md` for the
//! full capability inventory and round-1 scope decisions.

pub mod capabilities;
pub mod connector;
pub mod errors;
pub mod models;
pub mod secrets;

pub use capabilities::{AuthCapture, Capabilities, ItemKind};
pub use connector::{Connector, RunContext, CORE_API_VERSION};
pub use errors::{BackupRunError, ConnectorError, ConnectorLoadError, DbsError};
pub use models::{
    BackupItem, Checkpoint, ConnectorInfo, Cursor, DoctorCheck, FetchEvent, MaintenanceReport,
    MediaRef, ProgressEvent, ProgressPhase, ReconcileMarker, RestoreReport, RunResult, RunStatus,
    SourceStatus, VerifyIssue, VerifyReport,
};
pub use secrets::Secrets;
