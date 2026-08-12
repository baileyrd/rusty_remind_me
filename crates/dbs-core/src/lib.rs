//! Core data model and plugin contract for `rusty_dbs`.
//!
//! Mirrors `dbs.core` in baileyrd/Daily-Backup-System (pinned `@6cc6491`) —
//! see that repo's `docs/architecture.md` for the system this crate is
//! working toward parity with, and this repo's `gap-analysis.md` for the
//! full capability inventory and round-1 scope decisions.

pub mod cancel;
pub mod capabilities;
pub mod config;
pub mod connector;
pub mod engine;
pub mod errors;
pub mod export;
pub mod export_profile;
pub mod hashing;
pub mod http;
pub mod models;
pub mod netns;
pub mod registry;
pub mod secrets;
pub mod service;
pub mod storage;
pub mod timeutil;
pub mod versioning;

pub use cancel::CancelToken;
pub use capabilities::{AuthCapture, Capabilities, ItemKind};
pub use config::{
    load_config, parse_env_file, Config, ConnectorOverride, NotifyOn, SourceConfig, VpnGuard,
};
pub use connector::{Connector, RunContext, CORE_API_VERSION};
pub use engine::commit_checkpoint;
pub use errors::{BackupRunError, ConnectorError, ConnectorLoadError, DbsError};
pub use export::{
    available_formats, get_exporter, ArchiveExporter, CsvExporter, ExportResult, ExportSource,
    Exporter, JsonExporter, MarkdownExporter, NdjsonExporter, ObsidianExporter, WikiExporter,
};
pub use export_profile::{
    axis_label, group_values, raw_value, resolve_export_profile, ExportProfile,
    ExportProfileOverride, PAGE_PER,
};
pub use hashing::{canonical_json, content_hash};
pub use http::{HttpError, ManagedHttpClient};
pub use models::{
    BackupItem, Checkpoint, ConnectorInfo, Cursor, DoctorCheck, FetchEvent, MaintenanceReport,
    MediaRef, ProgressEvent, ProgressPhase, ReconcileMarker, RestoreReport, RunResult, RunStatus,
    SourceStatus, VerifyIssue, VerifyReport,
};
pub use netns::{in_named_netns, named_netns_exists};
pub use registry::{
    ConnectorCandidate, ConnectorRegistry, Handshake, LoadFailure, LoadReport, RegisteredConnector,
    DEFAULT_HANDSHAKE_TIMEOUT,
};
pub use secrets::Secrets;
pub use service::reap_once;
pub use storage::sqlite::open_connection;
pub use storage::sqlite_storage::SqliteStorage;
pub use storage::{BatchResult, ExportQuery, ItemRow, PreparedItem, SourceRecord, Storage};
pub use timeutil::{iso_z, parse_iso};
pub use versioning::{is_api_compatible, CURRENT_API_VERSION};
