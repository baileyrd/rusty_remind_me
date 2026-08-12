//! Newline-delimited JSON exporter — the canonical, lossless, streaming
//! format.
//!
//! Mirrors `src/dbs/export/ndjson.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). One JSON object per line. With
//! `ExportQuery::include_raw = true` (the default) each line embeds the
//! verbatim `raw` payload, making this format restore-grade.

use std::io::Write;

use crate::errors::DbsError;
use crate::storage::ExportQuery;

use super::{ExportResult, ExportSource, Exporter};

pub struct NdjsonExporter;

impl Exporter for NdjsonExporter {
    fn format(&self) -> &'static str {
        "ndjson"
    }

    fn media_type(&self) -> &'static str {
        "application/x-ndjson"
    }

    fn file_ext(&self) -> &'static str {
        ".ndjson"
    }

    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        _query: &ExportQuery,
    ) -> Result<ExportResult, DbsError> {
        let mut count: u64 = 0;
        let mut written: u64 = 0;
        for row in source.items() {
            let mut line = serde_json::to_string(&row)
                .map_err(|e| DbsError::Storage(format!("failed to encode export row: {e}")))?;
            line.push('\n');
            out.write_all(line.as_bytes())
                .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;
            written += line.len() as u64;
            count += 1;
        }
        Ok(ExportResult {
            format: self.format().to_string(),
            item_count: count,
            bytes_written: written,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ItemRow;
    use serde_json::Value;

    struct FakeSource {
        items: Vec<ItemRow>,
    }

    impl ExportSource for FakeSource {
        fn items(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.items.iter().cloned())
        }
        fn revisions(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(std::iter::empty())
        }
        fn media_blobs(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(std::iter::empty())
        }
        fn manifest(&self) -> ItemRow {
            ItemRow::new()
        }
    }

    fn row(external_id: &str) -> ItemRow {
        let mut r = ItemRow::new();
        r.insert("external_id".to_string(), Value::from(external_id));
        r
    }

    fn write_to_string(source: &FakeSource) -> (String, ExportResult) {
        let mut out: Vec<u8> = Vec::new();
        let result = NdjsonExporter
            .write(source, &mut out, &ExportQuery::default())
            .unwrap();
        (String::from_utf8(out).unwrap(), result)
    }

    #[test]
    fn empty_result_set_writes_nothing() {
        let source = FakeSource { items: Vec::new() };
        let (text, result) = write_to_string(&source);
        assert_eq!(text, "");
        assert_eq!(result.item_count, 0);
        assert_eq!(result.bytes_written, 0);
        assert_eq!(result.format, "ndjson");
    }

    #[test]
    fn single_item_is_one_line() {
        let source = FakeSource {
            items: vec![row("e1")],
        };
        let (text, result) = write_to_string(&source);
        assert_eq!(result.item_count, 1);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["external_id"], Value::from("e1"));
    }

    #[test]
    fn multiple_items_are_one_line_each_in_order() {
        let source = FakeSource {
            items: vec![row("e1"), row("e2"), row("e3")],
        };
        let (text, result) = write_to_string(&source);
        assert_eq!(result.item_count, 3);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        let ids: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["external_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
        assert_eq!(result.bytes_written, text.len() as u64);
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(NdjsonExporter.format(), "ndjson");
        assert_eq!(NdjsonExporter.media_type(), "application/x-ndjson");
        assert_eq!(NdjsonExporter.file_ext(), ".ndjson");
    }
}
