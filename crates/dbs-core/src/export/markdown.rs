//! Markdown exporter — a human-readable document grouped by source.
//!
//! Mirrors `src/dbs/export/markdown.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Great for skimming (e.g. a Raindrop bookmark
//! reading list). Lossy by design; use `ndjson`/`archive` for fidelity.

use std::io::Write;

use serde_json::Value;

use crate::errors::DbsError;
use crate::storage::ExportQuery;

use super::{ExportResult, ExportSource, Exporter};

/// Collapses any newlines/carriage returns/tabs to spaces so a title can
/// never break out of its heading line, then softens link brackets.
fn md_escape(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.replace('[', "\\[").replace(']', "\\]")
}

/// Python truthiness for a JSON value: `null`/`false`/`0`/empty
/// string/empty array/empty object are all falsy.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(value)` as Python would render it — used when a raw value is
/// interpolated into an f-string in the reference.
fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn truthy_field<'a>(row: &'a crate::storage::ItemRow, key: &str) -> Option<&'a Value> {
    row.get(key).filter(|v| is_truthy(v))
}

pub struct MarkdownExporter;

impl Exporter for MarkdownExporter {
    fn format(&self) -> &'static str {
        "markdown"
    }

    fn media_type(&self) -> &'static str {
        "text/markdown"
    }

    fn file_ext(&self) -> &'static str {
        ".md"
    }

    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        _query: &ExportQuery,
    ) -> Result<ExportResult, DbsError> {
        let mut count: u64 = 0;
        let mut written: u64 = 0;
        let mut emit = |text: &str| -> Result<(), DbsError> {
            out.write_all(text.as_bytes())
                .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;
            written += text.len() as u64;
            Ok(())
        };

        let mut current_source: Option<String> = None;

        emit("# Backup export\n")?;
        for row in source.items() {
            let src = truthy_field(&row, "source")
                .map(display_value)
                .unwrap_or_else(|| "(unknown)".to_string());
            if current_source.as_deref() != Some(src.as_str()) {
                current_source = Some(src.clone());
                emit(&format!("\n## {src}\n"))?;
            }

            let title = truthy_field(&row, "title")
                .or_else(|| truthy_field(&row, "url"))
                .or_else(|| truthy_field(&row, "external_id"))
                .map(display_value)
                .unwrap_or_else(|| "None".to_string());
            emit(&format!("\n### {}\n", md_escape(&title)))?;

            let mut meta: Vec<String> = Vec::new();
            if let Some(v) = truthy_field(&row, "item_kind") {
                meta.push(format!("kind: `{}`", display_value(v)));
            }
            if let Some(v) = truthy_field(&row, "created_at") {
                meta.push(format!("created: {}", display_value(v)));
            }
            if truthy_field(&row, "deleted").is_some() {
                meta.push("**deleted**".to_string());
            }
            if !meta.is_empty() {
                emit(&format!("_{}_\n", meta.join(" · ")))?;
            }

            if let Some(v) = truthy_field(&row, "url") {
                emit(&format!("\n<{}>\n", display_value(v)))?;
            }

            if let Some(Value::Array(tags)) = truthy_field(&row, "tags") {
                let rendered: Vec<String> = tags
                    .iter()
                    .map(|t| format!("`{}`", display_value(t)))
                    .collect();
                emit(&format!("\nTags: {}\n", rendered.join(", ")))?;
            }

            if let Some(v) = truthy_field(&row, "body") {
                emit(&format!("\n{}\n", display_value(v).trim()))?;
            }

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

    fn row(fields: &[(&str, Value)]) -> ItemRow {
        let mut r = ItemRow::new();
        for (k, v) in fields {
            r.insert(k.to_string(), v.clone());
        }
        r
    }

    fn write_to_string(source: &FakeSource) -> (String, ExportResult) {
        let mut out: Vec<u8> = Vec::new();
        let result = MarkdownExporter
            .write(source, &mut out, &ExportQuery::default())
            .unwrap();
        (String::from_utf8(out).unwrap(), result)
    }

    #[test]
    fn empty_result_set_only_has_the_top_level_heading() {
        let source = FakeSource { items: Vec::new() };
        let (text, result) = write_to_string(&source);
        assert_eq!(text, "# Backup export\n");
        assert_eq!(result.item_count, 0);
        assert_eq!(result.format, "markdown");
        assert_eq!(result.bytes_written, text.len() as u64);
    }

    #[test]
    fn single_item_renders_source_heading_title_and_body() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("title", json!("Hello World")),
                ("item_kind", json!("bookmark")),
                ("created_at", json!("2026-01-01T00:00:00Z")),
                ("url", json!("https://example.com")),
                ("tags", json!(["rust", "backup"])),
                ("body", json!("  some body text  ")),
            ])],
        };
        let (text, result) = write_to_string(&source);
        assert_eq!(result.item_count, 1);
        assert!(text.contains("## raindrop\n"));
        assert!(text.contains("### Hello World\n"));
        assert!(text.contains("kind: `bookmark`"));
        assert!(text.contains("created: 2026-01-01T00:00:00Z"));
        assert!(text.contains("<https://example.com>"));
        assert!(text.contains("Tags: `rust`, `backup`"));
        assert!(text.contains("\nsome body text\n"));
    }

    #[test]
    fn items_from_the_same_source_share_one_heading() {
        let source = FakeSource {
            items: vec![
                row(&[("source", json!("a")), ("title", json!("one"))]),
                row(&[("source", json!("a")), ("title", json!("two"))]),
                row(&[("source", json!("b")), ("title", json!("three"))]),
            ],
        };
        let (text, result) = write_to_string(&source);
        assert_eq!(result.item_count, 3);
        assert_eq!(text.matches("## a\n").count(), 1);
        assert_eq!(text.matches("## b\n").count(), 1);
    }

    #[test]
    fn deleted_item_is_flagged() {
        let source = FakeSource {
            items: vec![row(&[("title", json!("gone")), ("deleted", json!(true))])],
        };
        let (text, _result) = write_to_string(&source);
        assert!(text.contains("**deleted**"));
    }

    #[test]
    fn title_falls_back_to_url_then_external_id() {
        let source = FakeSource {
            items: vec![
                row(&[("url", json!("https://x.example"))]),
                row(&[("external_id", json!("ext-1"))]),
            ],
        };
        let (text, _result) = write_to_string(&source);
        assert!(text.contains("### https://x.example\n"));
        assert!(text.contains("### ext-1\n"));
    }

    #[test]
    fn title_is_escaped_and_flattened() {
        let source = FakeSource {
            items: vec![row(&[("title", json!("Line one\nLine [two]"))])],
        };
        let (text, _result) = write_to_string(&source);
        assert!(text.contains("### Line one Line \\[two\\]\n"));
    }

    #[test]
    fn missing_source_falls_back_to_unknown() {
        let source = FakeSource {
            items: vec![row(&[("title", json!("orphan"))])],
        };
        let (text, _result) = write_to_string(&source);
        assert!(text.contains("## (unknown)\n"));
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(MarkdownExporter.format(), "markdown");
        assert_eq!(MarkdownExporter.media_type(), "text/markdown");
        assert_eq!(MarkdownExporter.file_ext(), ".md");
    }
}
