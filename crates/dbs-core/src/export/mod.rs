//! Export abstractions: pluggable exporters keyed by format name.
//!
//! Mirrors `src/dbs/export/base.py` and `src/dbs/export/__init__.py` in
//! baileyrd/Daily-Backup-System (pinned `@6cc6491`). An [`Exporter`]
//! turns a stream of item rows into a portable file; a single
//! [`ExportQuery`](crate::storage::ExportQuery) filter object is shared
//! by the CLI today and a future web tier.
//!
//! **First exporter issue (#51, JSON) landed the shared base types**
//! (`ExportResult`/`ExportSource`/`Exporter`/`get_exporter`) that
//! `export/base.py` defines and #50 (the real `ExportQuery`)
//! deliberately deferred as "picked up by the individual exporter
//! issues." Every subsequent exporter issue (ndjson #53, csv #54,
//! markdown #55, obsidian #56, wiki #57, archive #58) adds its own
//! module plus one more arm to [`get_exporter`], same pattern as the
//! reference's `EXPORTERS` dict.
//!
//! Exporters stream from a storage iterator (so large datasets never
//! load fully into memory); writing via a temp file + atomic replace so
//! a crash mid-export never leaves a half-written file that looks
//! complete is the caller's responsibility (the CLI/service issue that
//! actually invokes `Exporter::write` against a real file), not this
//! trait's.

mod csv;
mod json;
mod ndjson;

use std::collections::HashMap;
use std::io::Write;

use serde_json::Value;

use crate::errors::DbsError;
use crate::storage::{ExportQuery, ItemRow};

pub use csv::CsvExporter;
pub use json::JsonExporter;
pub use ndjson::NdjsonExporter;

/// Summary of a completed export.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportResult {
    pub format: String,
    pub item_count: u64,
    pub revision_count: u64,
    pub bytes_written: u64,
    pub path: Option<String>,
    pub extra: HashMap<String, Value>,
}

/// A streaming data source handed to exporters.
///
/// Implemented by the service over storage + an `ExportQuery` (that
/// wiring is a separate CLI-facing issue, #70) — defined as a trait here
/// so this module doesn't depend on the storage layer, matching the
/// reference's structural `Protocol`.
pub trait ExportSource {
    fn items(&self) -> Box<dyn Iterator<Item = ItemRow> + '_>;
    fn revisions(&self) -> Box<dyn Iterator<Item = ItemRow> + '_>;
    fn media_blobs(&self) -> Box<dyn Iterator<Item = ItemRow> + '_>;
    fn manifest(&self) -> ItemRow;
}

/// Base contract every exporter implements.
pub trait Exporter {
    /// Stable format key, e.g. `"json"` — matches [`get_exporter`]'s
    /// lookup key and `ExportResult::format`.
    fn format(&self) -> &'static str;
    /// `Content-Type` a web tier would serve this format as. Declared
    /// now as the seam a future web layer would use; the CLI ignores it.
    fn media_type(&self) -> &'static str;
    /// File extension including the leading dot, e.g. `".json"`.
    fn file_ext(&self) -> &'static str;

    /// Streams from `source` to `out` and returns a summary.
    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        query: &ExportQuery,
    ) -> Result<ExportResult, DbsError>;
}

/// Looks up an exporter by format key. Mirrors the reference's
/// `get_exporter` — errors (rather than panics) on an unknown format,
/// listing what's actually available.
pub fn get_exporter(format: &str) -> Result<Box<dyn Exporter>, DbsError> {
    match format {
        "json" => Ok(Box::new(JsonExporter)),
        "ndjson" => Ok(Box::new(NdjsonExporter)),
        "csv" => Ok(Box::new(CsvExporter)),
        other => Err(DbsError::Config(format!(
            "unknown export format {other:?}. Available: {:?}",
            available_formats()
        ))),
    }
}

/// Every currently-registered format key, sorted — grows as each
/// exporter issue above lands.
pub fn available_formats() -> Vec<&'static str> {
    vec!["json", "ndjson", "csv"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_exporter_finds_json() {
        let exporter = get_exporter("json").unwrap();
        assert_eq!(exporter.format(), "json");
    }

    #[test]
    fn get_exporter_errors_on_an_unknown_format() {
        match get_exporter("bogus") {
            Ok(_) => panic!("expected an unknown-format error"),
            Err(e) => assert!(e.to_string().contains("bogus")),
        }
    }

    #[test]
    fn get_exporter_finds_ndjson() {
        let exporter = get_exporter("ndjson").unwrap();
        assert_eq!(exporter.format(), "ndjson");
    }

    #[test]
    fn get_exporter_finds_csv() {
        let exporter = get_exporter("csv").unwrap();
        assert_eq!(exporter.format(), "csv");
    }

    #[test]
    fn available_formats_lists_every_registered_format() {
        assert_eq!(available_formats(), vec!["json", "ndjson", "csv"]);
    }
}
