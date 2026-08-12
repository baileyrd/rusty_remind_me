//! Archive exporter — a self-describing zip bundle (the "take my data and
//! leave").
//!
//! Mirrors `src/dbs/export/archive.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Layout:
//! ```text
//! manifest.json                 # schema versions, query, counts, checksums
//! items/<source>.ndjson         # one NDJSON file per source (lossless with raw)
//! revisions/<source>.ndjson     # full change history (when include_revisions)
//! media/<source>/<id>/<file>    # archived media bytes (when any were stored)
//! ```
//!
//! Entries are written sequentially as the storage iterator yields them
//! (rows are assumed ordered by source), so the whole dataset is never
//! held in memory at once — each per-source `.ndjson` entry streams
//! straight to the open zip entry, one line at a time.
//!
//! The manifest carries a sha256 per entry (`checksums`), computed while
//! streaming, so the bundle is self-*verifying*, not just
//! self-describing — a later `dbs verify --archive`-equivalent checks
//! it, and restore should refuse a bundle whose bytes no longer match
//! before ingesting anything.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Seek, Write};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::errors::DbsError;
use crate::storage::{ExportQuery, ItemRow};
use crate::timeutil::iso_z;

use super::{ExportResult, ExportSource, Exporter};

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

fn truthy_field<'a>(row: &'a ItemRow, key: &str) -> Option<&'a Value> {
    row.get(key).filter(|v| is_truthy(v))
}

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

fn bytes_from_value(value: &Value) -> Option<Vec<u8>> {
    let arr = value.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(u8::try_from(item.as_u64()?).ok()?);
    }
    Some(out)
}

/// Replaces every run of characters outside `[A-Za-z0-9._-]` with a
/// single `_`, strips leading/trailing `_`, and falls back to
/// `"source"` if that leaves nothing.
fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "source".to_string()
    } else {
        trimmed.to_string()
    }
}

pub struct ArchiveExporter;

impl Exporter for ArchiveExporter {
    fn format(&self) -> &'static str {
        "archive"
    }

    fn media_type(&self) -> &'static str {
        "application/zip"
    }

    fn file_ext(&self) -> &'static str {
        ".zip"
    }

    fn write(
        &self,
        source: &dyn ExportSource,
        out: &mut dyn Write,
        query: &ExportQuery,
    ) -> Result<ExportResult, DbsError> {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut zf = ZipWriter::new(Cursor::new(Vec::new()));
        let mut checksums: HashMap<String, String> = HashMap::new();

        let (item_count, by_source) =
            write_grouped(&mut zf, "items", source.items(), &mut checksums, options)?;
        let revision_count = if query.include_revisions {
            let (count, _) = write_grouped(
                &mut zf,
                "revisions",
                source.revisions(),
                &mut checksums,
                options,
            )?;
            count
        } else {
            0
        };
        let media_count = write_media(&mut zf, source.media_blobs(), &mut checksums, options)?;

        let manifest = build_manifest(
            source.manifest(),
            query,
            item_count,
            revision_count,
            media_count,
            &by_source,
            &checksums,
        );
        let manifest_text = serde_json::to_string_pretty(&manifest)
            .map_err(|e| DbsError::Storage(format!("failed to encode export manifest: {e}")))?;
        zf.start_file("manifest.json", options).map_err(zip_err)?;
        zf.write_all(manifest_text.as_bytes()).map_err(io_err)?;

        let cursor = zf.finish().map_err(zip_err)?;
        out.write_all(&cursor.into_inner()).map_err(io_err)?;

        let extra = HashMap::from([
            (
                "by_source".to_string(),
                serde_json::to_value(&by_source).unwrap_or(Value::Null),
            ),
            ("media".to_string(), Value::from(media_count)),
        ]);

        Ok(ExportResult {
            format: self.format().to_string(),
            item_count,
            revision_count,
            extra,
            ..Default::default()
        })
    }
}

/// Streams `rows` into one `<folder>/<slugified source>.ndjson` zip
/// entry per source, computing a running sha256 per entry as it goes.
/// Rows are assumed ordered by source (the caller's storage iterator
/// guarantee); a source change closes the previous entry and opens the
/// next.
fn write_grouped<W: Write + Seek>(
    zf: &mut ZipWriter<W>,
    folder: &str,
    rows: Box<dyn Iterator<Item = ItemRow> + '_>,
    checksums: &mut HashMap<String, String>,
    options: SimpleFileOptions,
) -> Result<(u64, HashMap<String, u64>), DbsError> {
    let mut total: u64 = 0;
    let mut by_source: HashMap<String, u64> = HashMap::new();
    let mut current: Option<String> = None;
    let mut entry_name: Option<String> = None;
    let mut digest = Sha256::new();

    for row in rows {
        let src = truthy_field(&row, "source")
            .map(display_value)
            .unwrap_or_else(|| "unknown".to_string());
        if current.as_deref() != Some(src.as_str()) {
            if let Some(name) = entry_name.take() {
                let finished = std::mem::replace(&mut digest, Sha256::new());
                checksums.insert(name, format!("{:x}", finished.finalize()));
            }
            current = Some(src.clone());
            let name = format!("{folder}/{}.ndjson", slug(&src));
            zf.start_file(&name, options).map_err(zip_err)?;
            entry_name = Some(name);
        }
        let mut line = serde_json::to_string(&row)
            .map_err(|e| DbsError::Storage(format!("failed to encode export row: {e}")))?;
        line.push('\n');
        zf.write_all(line.as_bytes()).map_err(io_err)?;
        digest.update(line.as_bytes());
        *by_source.entry(src).or_insert(0) += 1;
        total += 1;
    }
    if let Some(name) = entry_name.take() {
        checksums.insert(name, format!("{:x}", digest.finalize()));
    }

    Ok((total, by_source))
}

/// Writes each stored media blob to `media/<source>/<external_id>/<file>`.
fn write_media<W: Write + Seek>(
    zf: &mut ZipWriter<W>,
    rows: Box<dyn Iterator<Item = ItemRow> + '_>,
    checksums: &mut HashMap<String, String>,
    options: SimpleFileOptions,
) -> Result<u64, DbsError> {
    let mut count: u64 = 0;
    let mut seen: HashSet<String> = HashSet::new();

    for row in rows {
        let data = match row.get("data").and_then(bytes_from_value) {
            Some(d) if !d.is_empty() => d,
            _ => continue,
        };
        let src = slug(
            &truthy_field(&row, "source")
                .map(display_value)
                .unwrap_or_else(|| "unknown".to_string()),
        );
        let ext_id = slug(
            &truthy_field(&row, "external_id")
                .map(display_value)
                .unwrap_or_else(|| "item".to_string()),
        );
        let fname = slug(
            &truthy_field(&row, "filename")
                .or_else(|| truthy_field(&row, "sha256"))
                .map(display_value)
                .unwrap_or_else(|| "file".to_string()),
        );
        let mut path = format!("media/{src}/{ext_id}/{fname}");
        if seen.contains(&path) {
            let sha = truthy_field(&row, "sha256")
                .map(display_value)
                .unwrap_or_else(|| count.to_string());
            let sha8: String = sha.chars().take(8).collect();
            path = format!("media/{src}/{ext_id}/{sha8}_{fname}");
        }
        seen.insert(path.clone());

        zf.start_file(&path, options).map_err(zip_err)?;
        zf.write_all(&data).map_err(io_err)?;

        // Computed fresh — the stored sha256 column is trusted nowhere here.
        let mut hasher = Sha256::new();
        hasher.update(&data);
        checksums.insert(path, format!("{:x}", hasher.finalize()));
        count += 1;
    }

    Ok(count)
}

#[allow(clippy::too_many_arguments)]
fn build_manifest(
    manifest: ItemRow,
    query: &ExportQuery,
    item_count: u64,
    revision_count: u64,
    media_count: u64,
    by_source: &HashMap<String, u64>,
    checksums: &HashMap<String, String>,
) -> Value {
    let mut map: Map<String, Value> = manifest.into_iter().collect();
    map.insert(
        "query".to_string(),
        json!({
            "sources": query.sources,
            "item_types": query.item_types,
            "since": query.since.map(iso_z),
            "until": query.until.map(iso_z),
            "include_deleted": query.include_deleted,
            "include_revisions": query.include_revisions,
            "include_raw": query.include_raw,
        }),
    );
    map.insert(
        "counts".to_string(),
        json!({
            "items": item_count,
            "revisions": revision_count,
            "media": media_count,
            "by_source": by_source,
        }),
    );
    map.insert(
        "checksum_algorithm".to_string(),
        Value::String("sha256".to_string()),
    );
    map.insert(
        "checksums".to_string(),
        serde_json::to_value(checksums).unwrap_or(Value::Null),
    );
    Value::Object(map)
}

fn zip_err(e: zip::result::ZipError) -> DbsError {
    DbsError::Storage(format!("failed to write export: {e}"))
}

fn io_err(e: std::io::Error) -> DbsError {
    DbsError::Storage(format!("failed to write export: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    struct FakeSource {
        items: Vec<ItemRow>,
        revisions: Vec<ItemRow>,
        media_blobs: Vec<ItemRow>,
    }

    impl ExportSource for FakeSource {
        fn items(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.items.iter().cloned())
        }
        fn revisions(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.revisions.iter().cloned())
        }
        fn media_blobs(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.media_blobs.iter().cloned())
        }
        fn manifest(&self) -> ItemRow {
            let mut m = ItemRow::new();
            m.insert("db_schema_version".to_string(), json!(3));
            m
        }
    }

    fn row(fields: &[(&str, Value)]) -> ItemRow {
        let mut r = ItemRow::new();
        for (k, v) in fields {
            r.insert(k.to_string(), v.clone());
        }
        r
    }

    fn write_zip(
        source: &FakeSource,
        query: &ExportQuery,
    ) -> (zip::ZipArchive<Cursor<Vec<u8>>>, ExportResult) {
        let mut out: Vec<u8> = Vec::new();
        let result = ArchiveExporter.write(source, &mut out, query).unwrap();
        let archive = zip::ZipArchive::new(Cursor::new(out)).unwrap();
        (archive, result)
    }

    fn read_entry(archive: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> String {
        let mut file = archive.by_name(name).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn empty_result_set_writes_only_a_manifest() {
        let source = FakeSource {
            items: Vec::new(),
            revisions: Vec::new(),
            media_blobs: Vec::new(),
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 0);
        assert_eq!(result.format, "archive");
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert_eq!(names, vec!["manifest.json".to_string()]);
        let manifest = read_entry(&mut archive, "manifest.json");
        let parsed: Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["counts"]["items"], 0);
        assert_eq!(parsed["db_schema_version"], 3);
        assert_eq!(parsed["checksum_algorithm"], "sha256");
    }

    #[test]
    fn single_item_writes_one_ndjson_entry_with_a_verified_checksum() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("title", json!("hello")),
            ])],
            revisions: Vec::new(),
            media_blobs: Vec::new(),
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 1);
        let entry_text = read_entry(&mut archive, "items/raindrop.ndjson");
        let lines: Vec<&str> = entry_text.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["external_id"], "e1");

        let manifest = read_entry(&mut archive, "manifest.json");
        let manifest_json: Value = serde_json::from_str(&manifest).unwrap();
        let expected_checksum = {
            let mut hasher = Sha256::new();
            hasher.update(entry_text.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        assert_eq!(
            manifest_json["checksums"]["items/raindrop.ndjson"],
            Value::String(expected_checksum)
        );
    }

    #[test]
    fn revisions_are_only_written_when_the_query_asks_for_them() {
        let source = FakeSource {
            items: Vec::new(),
            revisions: vec![row(&[
                ("source", json!("raindrop")),
                ("revision", json!(1)),
            ])],
            media_blobs: Vec::new(),
        };
        let (archive_off, result_off) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result_off.revision_count, 0);
        assert!(!archive_off
            .file_names()
            .any(|n| n.starts_with("revisions/")));

        let query_on = ExportQuery {
            include_revisions: true,
            ..ExportQuery::default()
        };
        let (archive_on, result_on) = write_zip(&source, &query_on);
        assert_eq!(result_on.revision_count, 1);
        assert!(archive_on
            .file_names()
            .any(|n| n == "revisions/raindrop.ndjson"));
    }

    #[test]
    fn multiple_sources_get_separate_entries_and_by_source_counts() {
        let source = FakeSource {
            items: vec![
                row(&[("source", json!("a")), ("external_id", json!("1"))]),
                row(&[("source", json!("a")), ("external_id", json!("2"))]),
                row(&[("source", json!("b")), ("external_id", json!("3"))]),
            ],
            revisions: Vec::new(),
            media_blobs: Vec::new(),
        };
        let (archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 3);
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert!(names.contains(&"items/a.ndjson".to_string()));
        assert!(names.contains(&"items/b.ndjson".to_string()));
        assert_eq!(
            result.extra.get("by_source"),
            Some(&json!({"a": 2, "b": 1}))
        );
    }

    #[test]
    fn media_blob_checksum_round_trips() {
        let source = FakeSource {
            items: Vec::new(),
            revisions: Vec::new(),
            media_blobs: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("filename", json!("photo.png")),
                ("data", json!([1, 2, 3, 4])),
            ])],
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.extra.get("media"), Some(&Value::from(1)));
        let mut file = archive.by_name("media/raindrop/e1/photo.png").unwrap();
        let mut data = Vec::new();
        file.read_to_end(&mut data).unwrap();
        assert_eq!(data, vec![1u8, 2, 3, 4]);
        drop(file);

        let manifest = read_entry(&mut archive, "manifest.json");
        let manifest_json: Value = serde_json::from_str(&manifest).unwrap();
        let mut hasher = Sha256::new();
        hasher.update([1u8, 2, 3, 4]);
        let expected = format!("{:x}", hasher.finalize());
        assert_eq!(
            manifest_json["checksums"]["media/raindrop/e1/photo.png"],
            Value::String(expected)
        );
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(ArchiveExporter.format(), "archive");
        assert_eq!(ArchiveExporter.media_type(), "application/zip");
        assert_eq!(ArchiveExporter.file_ext(), ".zip");
    }
}
