//! Single pretty-JSON-document exporter.
//!
//! Mirrors `src/dbs/export/json.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Emits one JSON array of item objects —
//! convenient for small exports and human reading; for very large
//! datasets prefer the (separately filed) `ndjson` exporter, streamed
//! line-by-line. The array brackets/commas are streamed so this still
//! avoids building one giant string in memory, same as the reference.

use std::io::Write;

use crate::errors::DbsError;
use crate::storage::ExportQuery;

use super::{ExportResult, ExportSource, Exporter};

pub struct JsonExporter;

impl Exporter for JsonExporter {
    fn format(&self) -> &'static str {
        "json"
    }

    fn media_type(&self) -> &'static str {
        "application/json"
    }

    fn file_ext(&self) -> &'static str {
        ".json"
    }

    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        _query: &ExportQuery,
    ) -> Result<ExportResult, DbsError> {
        let mut count: u64 = 0;
        let mut written: u64 = 0;
        let mut emit = |data: &[u8]| -> Result<(), DbsError> {
            out.write_all(data)
                .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;
            written += data.len() as u64;
            Ok(())
        };

        emit(b"[\n")?;
        let mut first = true;
        for row in source.items() {
            emit(if first { b"" } else { b",\n" })?;
            first = false;
            let text = serde_json::to_string_pretty(&row)
                .map_err(|e| DbsError::Storage(format!("failed to encode export row: {e}")))?;
            emit(text.as_bytes())?;
            count += 1;
        }
        emit(if count > 0 { b"\n]\n" } else { b"]\n" })?;

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
        let result = JsonExporter
            .write(source, &mut out, &ExportQuery::default())
            .unwrap();
        (String::from_utf8(out).unwrap(), result)
    }

    #[test]
    fn empty_result_set_emits_an_empty_array() {
        let source = FakeSource { items: Vec::new() };
        let (text, result) = write_to_string(&source);
        assert_eq!(text, "[\n]\n");
        assert_eq!(result.item_count, 0);
        assert_eq!(result.format, "json");
        assert_eq!(result.bytes_written, text.len() as u64);
    }

    #[test]
    fn single_item_round_trips_through_the_json_array() {
        let source = FakeSource {
            items: vec![row("e1")],
        };
        let (text, result) = write_to_string(&source);
        assert_eq!(result.item_count, 1);

        let parsed: Value = serde_json::from_str(&text).unwrap();
        let array = parsed.as_array().unwrap();
        assert_eq!(array.len(), 1);
        assert_eq!(array[0]["external_id"], Value::from("e1"));
    }

    #[test]
    fn multiple_items_are_comma_separated_and_all_present() {
        let source = FakeSource {
            items: vec![row("e1"), row("e2"), row("e3")],
        };
        let (text, result) = write_to_string(&source);
        assert_eq!(result.item_count, 3);

        let parsed: Value = serde_json::from_str(&text).unwrap();
        let array = parsed.as_array().unwrap();
        assert_eq!(array.len(), 3);
        let ids: Vec<&str> = array
            .iter()
            .map(|v| v["external_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(JsonExporter.format(), "json");
        assert_eq!(JsonExporter.media_type(), "application/json");
        assert_eq!(JsonExporter.file_ext(), ".json");
    }
}
