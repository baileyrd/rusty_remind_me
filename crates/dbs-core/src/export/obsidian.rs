//! Obsidian-vault-style markdown exporter — one `.md` note per item, zipped.
//!
//! Mirrors `src/dbs/export/obsidian.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Produces frontmatter compatible with the
//! reference author's own url2obs clipper convention: YAML frontmatter
//! (category/author/title/description/source/clipped/published/tags)
//! followed by the item's body. DBS's own provenance fields use a
//! `dbs_` prefix (`dbs_source`, `dbs_external_id`, ...) specifically to
//! avoid colliding with url2obs's `source:` key, which means "the
//! original article URL" in that convention — not "the DBS source name".
//!
//! Layout inside the zip:
//! ```text
//! notes/<slug>.md                     # one note per (live) item
//! media/<source>/<external_id>/<file> # archived permanent-copy blobs, when present
//! manifest.json                       # same shape as the archive exporter's (#58)
//! ```
//!
//! Deleted items are excluded by the usual `include_deleted` query
//! filter, same as every other exporter; if one slips through via an
//! explicit `include_deleted` export it is still written but flagged in
//! its frontmatter rather than silently vanishing.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Seek, Write};

use serde_json::{json, Map, Value};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::errors::DbsError;
use crate::storage::{ExportQuery, ItemRow};
use crate::timeutil::iso_z;

use super::{ExportResult, ExportSource, Exporter};

/// Python truthiness for a JSON value.
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

/// Replaces every run of characters outside `[A-Za-z0-9._-]` with a
/// single `_`, strips leading/trailing `_`, and falls back to `"item"`
/// if that leaves nothing.
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
        "item".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A YAML-safe double-quoted scalar. Double-quoted style is used
/// unconditionally so callers never need to reason about which
/// characters are "safe" in plain scalars; newlines/tabs are collapsed
/// to single spaces since frontmatter values here are single-line by
/// convention.
fn yaml_scalar(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(v) => display_value(v),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped = flat.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn yaml_list(values: &[Value]) -> String {
    if values.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = values.iter().map(|v| yaml_scalar(Some(v))).collect();
    format!("[{}]", items.join(", "))
}

fn looks_embeddable(mime: Option<&str>) -> bool {
    let mime = mime.unwrap_or("").to_ascii_lowercase();
    mime.starts_with("image/") || mime == "application/pdf"
}

fn bytes_from_value(value: &Value) -> Option<Vec<u8>> {
    let arr = value.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(u8::try_from(item.as_u64()?).ok()?);
    }
    Some(out)
}

struct MediaEntry {
    zip_name: String,
    mime: Option<String>,
}

/// Keyed by `(source, external_id)`.
type MediaIndex = HashMap<(String, String), Vec<MediaEntry>>;

pub struct ObsidianExporter;

impl Exporter for ObsidianExporter {
    fn format(&self) -> &'static str {
        "obsidian"
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

        let (media_count, media_index) = write_media(&mut zf, source.media_blobs(), options)?;

        let mut by_source: HashMap<String, u64> = HashMap::new();
        let mut seen_names: HashSet<String> = HashSet::new();
        let empty_media: Vec<MediaEntry> = Vec::new();
        let mut item_count: u64 = 0;
        for row in source.items() {
            let src = truthy_field(&row, "source")
                .map(display_value)
                .unwrap_or_else(|| "unknown".to_string());
            let ext_id = truthy_field(&row, "external_id")
                .map(display_value)
                .unwrap_or_else(|| "item".to_string());
            let name = note_filename(&row, &mut seen_names);
            let media_rows = media_index
                .get(&(src.clone(), ext_id.clone()))
                .unwrap_or(&empty_media);
            let body = render_note(&row, media_rows);
            zf.start_file(format!("notes/{name}"), options)
                .map_err(zip_err)?;
            zf.write_all(body.as_bytes())
                .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;
            *by_source.entry(src).or_insert(0) += 1;
            item_count += 1;
        }

        let manifest = build_manifest(
            source.manifest(),
            query,
            item_count,
            media_count,
            &by_source,
        );
        let manifest_text = serde_json::to_string_pretty(&manifest)
            .map_err(|e| DbsError::Storage(format!("failed to encode export manifest: {e}")))?;
        zf.start_file("manifest.json", options).map_err(zip_err)?;
        zf.write_all(manifest_text.as_bytes())
            .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;

        let cursor = zf.finish().map_err(zip_err)?;
        out.write_all(&cursor.into_inner())
            .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;

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
            extra,
            ..Default::default()
        })
    }
}

fn note_filename(row: &ItemRow, seen: &mut HashSet<String>) -> String {
    let title = truthy_field(row, "title")
        .or_else(|| truthy_field(row, "url"))
        .or_else(|| truthy_field(row, "external_id"))
        .map(display_value)
        .unwrap_or_default();
    let base: String = slug(&title).chars().take(80).collect();
    let ext_id = slug(
        &truthy_field(row, "external_id")
            .map(display_value)
            .unwrap_or_default(),
    );

    let candidate = format!("{base}.md");
    if seen.insert(candidate.clone()) {
        return candidate;
    }
    let candidate = format!("{base}-{ext_id}.md");
    if seen.insert(candidate.clone()) {
        return candidate;
    }
    let src = slug(
        &truthy_field(row, "source")
            .map(display_value)
            .unwrap_or_default(),
    );
    let candidate = format!("{base}-{src}-{ext_id}.md");
    seen.insert(candidate.clone());
    candidate
}

fn render_note(row: &ItemRow, media_rows: &[MediaEntry]) -> String {
    let title = truthy_field(row, "title")
        .or_else(|| truthy_field(row, "url"))
        .or_else(|| truthy_field(row, "external_id"));
    let created_at = row.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
    let clipped: String = created_at.chars().take(10).collect();
    let clipped_value = if clipped.is_empty() {
        None
    } else {
        Some(Value::String(clipped))
    };
    let tags: Vec<Value> = row
        .get("tags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut lines: Vec<String> = vec!["---".to_string()];
    lines.push("category: \"[[Clippings]]\"".to_string());
    lines.push(format!("author: {}", yaml_scalar(None)));
    lines.push(format!("title: {}", yaml_scalar(title)));
    lines.push(format!("description: {}", yaml_scalar(row.get("body"))));
    lines.push(format!("source: {}", yaml_scalar(row.get("url"))));
    lines.push(format!("clipped: {}", yaml_scalar(clipped_value.as_ref())));
    lines.push(format!("published: {}", yaml_scalar(None)));
    lines.push(format!("tags: {}", yaml_list(&tags)));
    lines.push(format!("dbs_source: {}", yaml_scalar(row.get("source"))));
    lines.push(format!(
        "dbs_external_id: {}",
        yaml_scalar(row.get("external_id"))
    ));
    lines.push(format!(
        "dbs_item_kind: {}",
        yaml_scalar(row.get("item_kind"))
    ));
    if truthy_field(row, "deleted").is_some() {
        lines.push("dbs_deleted: true".to_string());
    }
    lines.push("---".to_string());
    lines.push(String::new());
    if let Some(body) = truthy_field(row, "body") {
        lines.push(display_value(body).trim().to_string());
        lines.push(String::new());
    }
    if !media_rows.is_empty() {
        lines.push("## Archived copy".to_string());
        for m in media_rows {
            if looks_embeddable(m.mime.as_deref()) {
                lines.push(format!("- ![[{}]]", m.zip_name));
            } else {
                lines.push(format!("- [{}]({})", m.zip_name, m.zip_name));
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

/// Writes each stored media blob to `media/<source>/<external_id>/<file>`.
fn write_media<W: Write + Seek>(
    zf: &mut ZipWriter<W>,
    rows: Box<dyn Iterator<Item = ItemRow> + '_>,
    options: SimpleFileOptions,
) -> Result<(u64, MediaIndex), DbsError> {
    let mut count: u64 = 0;
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut index: MediaIndex = HashMap::new();

    for row in rows {
        let data = match row.get("data").and_then(bytes_from_value) {
            Some(d) if !d.is_empty() => d,
            _ => continue,
        };
        let src = truthy_field(&row, "source")
            .map(display_value)
            .unwrap_or_else(|| "unknown".to_string());
        let ext_id = truthy_field(&row, "external_id")
            .map(display_value)
            .unwrap_or_else(|| "item".to_string());
        let mut fname = slug(
            &truthy_field(&row, "filename")
                .or_else(|| truthy_field(&row, "sha256"))
                .map(display_value)
                .unwrap_or_else(|| "file".to_string()),
        );
        let src_slug = slug(&src);
        let ext_id_slug = slug(&ext_id);
        let mut path = format!("media/{src_slug}/{ext_id_slug}/{fname}");
        if seen_paths.contains(&path) {
            let sha = truthy_field(&row, "sha256")
                .map(display_value)
                .unwrap_or_else(|| count.to_string());
            let sha8: String = sha.chars().take(8).collect();
            fname = format!("{sha8}_{fname}");
            path = format!("media/{src_slug}/{ext_id_slug}/{fname}");
        }
        seen_paths.insert(path.clone());

        zf.start_file(&path, options).map_err(zip_err)?;
        zf.write_all(&data)
            .map_err(|e| DbsError::Storage(format!("failed to write export: {e}")))?;

        let mime = row
            .get("mime")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        index.entry((src, ext_id)).or_default().push(MediaEntry {
            zip_name: path,
            mime,
        });
        count += 1;
    }

    Ok((count, index))
}

fn build_manifest(
    manifest: ItemRow,
    query: &ExportQuery,
    item_count: u64,
    media_count: u64,
    by_source: &HashMap<String, u64>,
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
            "media": media_count,
            "by_source": by_source,
        }),
    );
    Value::Object(map)
}

fn zip_err(e: zip::result::ZipError) -> DbsError {
    DbsError::Storage(format!("failed to write export: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    struct FakeSource {
        items: Vec<ItemRow>,
        media_blobs: Vec<ItemRow>,
    }

    impl ExportSource for FakeSource {
        fn items(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.items.iter().cloned())
        }
        fn revisions(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(std::iter::empty())
        }
        fn media_blobs(&self) -> Box<dyn Iterator<Item = ItemRow> + '_> {
            Box::new(self.media_blobs.iter().cloned())
        }
        fn manifest(&self) -> ItemRow {
            let mut m = ItemRow::new();
            m.insert("generated_at".to_string(), json!("2026-01-01T00:00:00Z"));
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
        let result = ObsidianExporter.write(source, &mut out, query).unwrap();
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
    fn empty_result_set_still_writes_a_manifest() {
        let source = FakeSource {
            items: Vec::new(),
            media_blobs: Vec::new(),
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 0);
        assert_eq!(result.format, "obsidian");
        let manifest = read_entry(&mut archive, "manifest.json");
        let parsed: Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["counts"]["items"], 0);
        assert_eq!(parsed["counts"]["media"], 0);
    }

    #[test]
    fn single_item_becomes_one_note_with_frontmatter() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("title", json!("Hello, World")),
                ("url", json!("https://example.com")),
                ("body", json!("  some body  ")),
                ("item_kind", json!("bookmark")),
                ("created_at", json!("2026-01-02T03:04:05Z")),
                ("tags", json!(["rust", "backup"])),
            ])],
            media_blobs: Vec::new(),
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 1);
        let note = read_entry(&mut archive, "notes/Hello_World.md");
        assert!(note.contains("title: \"Hello, World\""));
        assert!(note.contains("source: \"https://example.com\""));
        assert!(note.contains("clipped: \"2026-01-02\""));
        assert!(note.contains("tags: [\"rust\", \"backup\"]"));
        assert!(note.contains("dbs_source: \"raindrop\""));
        assert!(note.contains("dbs_external_id: \"e1\""));
        assert!(note.contains("dbs_item_kind: \"bookmark\""));
        assert!(note.contains("some body"));
        assert!(!note.contains("dbs_deleted"));
    }

    #[test]
    fn deleted_item_is_flagged_in_frontmatter() {
        let source = FakeSource {
            items: vec![row(&[
                ("external_id", json!("e1")),
                ("title", json!("gone")),
                ("deleted", json!(true)),
            ])],
            media_blobs: Vec::new(),
        };
        let (mut archive, _result) = write_zip(&source, &ExportQuery::default());
        let note = read_entry(&mut archive, "notes/gone.md");
        assert!(note.contains("dbs_deleted: true"));
    }

    #[test]
    fn colliding_titles_are_disambiguated_by_external_id() {
        let source = FakeSource {
            items: vec![
                row(&[("external_id", json!("e1")), ("title", json!("same"))]),
                row(&[("external_id", json!("e2")), ("title", json!("same"))]),
            ],
            media_blobs: Vec::new(),
        };
        let (archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.item_count, 2);
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert!(names.contains(&"notes/same.md".to_string()));
        assert!(names.contains(&"notes/same-e2.md".to_string()));
    }

    #[test]
    fn media_blob_is_written_and_linked_from_its_note() {
        let source = FakeSource {
            items: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("title", json!("with media")),
            ])],
            media_blobs: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("filename", json!("photo.png")),
                ("mime", json!("image/png")),
                ("data", json!([1, 2, 3])),
            ])],
        };
        let (mut archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.extra.get("media"), Some(&Value::from(1)));
        let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
        assert!(names.iter().any(|n| n.starts_with("media/raindrop/e1/")));
        let note = read_entry(&mut archive, "notes/with_media.md");
        assert!(note.contains("## Archived copy"));
        assert!(note.contains("![["));
    }

    #[test]
    fn media_blob_without_data_is_skipped() {
        let source = FakeSource {
            items: Vec::new(),
            media_blobs: vec![row(&[
                ("source", json!("raindrop")),
                ("external_id", json!("e1")),
                ("filename", json!("photo.png")),
            ])],
        };
        let (archive, result) = write_zip(&source, &ExportQuery::default());
        assert_eq!(result.extra.get("media"), Some(&Value::from(0)));
        assert!(archive.file_names().all(|n| !n.starts_with("media/")));
    }

    #[test]
    fn exporter_metadata_matches_the_reference() {
        assert_eq!(ObsidianExporter.format(), "obsidian");
        assert_eq!(ObsidianExporter.media_type(), "application/zip");
        assert_eq!(ObsidianExporter.file_ext(), ".zip");
    }
}
