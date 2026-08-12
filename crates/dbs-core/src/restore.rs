//! Read a backup export back into the database.
//!
//! Mirrors `src/dbs/restore.py` in baileyrd/Daily-Backup-System (pinned
//! `@6cc6491`). The system's lossless claim lived entirely on the write
//! side (`ndjson` is "restore-grade", `archive` "self-describing") with
//! nothing able to read either back — this module closes the loop: rows
//! are replayed through the same classified `upsert_items` path a live
//! backup uses ([`crate::service::BackupService::restore`]), so restore
//! gets idempotency and change classification for free.
//!
//! Two deliberate choices, carried over from the reference:
//!
//! * **The stored `content_hash` is carried over verbatim, never
//!   recomputed.** Recomputing would need each connector's
//!   `volatile_fields` — i.e. the connector installed — while carrying
//!   it over keeps restore fully connector-independent and makes
//!   re-restoring the same bundle a no-op (every row classifies
//!   "unchanged").
//! * **Latest item state only (v1).** Revision history and media blobs
//!   present in an archive bundle are counted and reported as
//!   *skipped*, not restored — replaying revisions verbatim would
//!   bypass the engine's one-revision-per-change invariant, and media
//!   rows need their items' DB ids; both are better done deliberately
//!   later than approximately now.
//!
//! **Documented divergence:** the reference's `iter_export_rows` is a
//! generator (rows stream one line at a time, so a multi-GB bundle
//! never loads fully into memory); this port collects into a `Vec`
//! instead. Managing a `zip::read::ZipFile`'s borrow across a lazy
//! Rust iterator adds real complexity for no behavior this issue's own
//! acceptance criteria exercise (no test needs memory-boundedness) —
//! flagged here rather than silently narrowed.
//!
//! Only [`crate::service::BackupService::restore`] calls the functions
//! here; they do parsing/mapping and never touch storage themselves,
//! matching the reference's own module split.

use std::io::Read;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::errors::DbsError;
use crate::storage::PreparedItem;

fn io_err(e: std::io::Error) -> DbsError {
    DbsError::Storage(format!("restore I/O failed: {e}"))
}

fn zip_err(e: zip::result::ZipError) -> DbsError {
    DbsError::Config(format!("not a valid dbs archive: {e}"))
}

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

/// `None` for a missing key or an explicit JSON `null` — both read as
/// Python's `dict.get` returning `None`.
fn opt_display(value: Option<&Value>) -> Option<String> {
    value.filter(|v| !v.is_null()).map(display_value)
}

/// The archive's `manifest.json`, or `None` for a bare ndjson file.
///
/// A zip without a manifest is refused outright — it is not a dbs
/// archive, and guessing at its layout risks restoring garbage.
pub fn read_manifest(path: &Path) -> Result<Option<Value>, DbsError> {
    let file = std::fs::File::open(path).map_err(io_err)?;
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return Ok(None),
    };
    let result = match archive.by_name("manifest.json") {
        Ok(mut entry) => {
            let mut text = String::new();
            entry.read_to_string(&mut text).map_err(io_err)?;
            let manifest: Value = serde_json::from_str(&text).map_err(|e| {
                DbsError::Config(format!(
                    "{}: manifest.json is not valid JSON: {e}",
                    path.display()
                ))
            })?;
            Ok(Some(manifest))
        }
        Err(_) => Err(DbsError::Config(format!(
            "{} is a zip but has no manifest.json — not a dbs archive (expected a bundle written by an archive export).",
            path.display()
        ))),
    };
    result
}

/// Every item row from an archive zip (`items/*.ndjson`) or a bare
/// ndjson export, in source-file order.
pub fn iter_export_rows(path: &Path) -> Result<Vec<Value>, DbsError> {
    let file = std::fs::File::open(path).map_err(io_err)?;
    match zip::ZipArchive::new(file) {
        Ok(mut archive) => {
            let mut names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
            names.retain(|n| n.starts_with("items/") && n.ends_with(".ndjson"));
            names.sort();
            if names.is_empty() {
                return Err(DbsError::Config(format!(
                    "{}: archive contains no items/*.ndjson",
                    path.display()
                )));
            }
            let mut rows = Vec::new();
            for name in names {
                let mut entry = archive.by_name(&name).map_err(zip_err)?;
                let mut text = String::new();
                entry.read_to_string(&mut text).map_err(io_err)?;
                let where_ = format!("{}!{name}", path.display());
                rows.extend(parse_ndjson_lines(&text, &where_)?);
            }
            Ok(rows)
        }
        Err(_) => {
            let text = std::fs::read_to_string(path).map_err(io_err)?;
            parse_ndjson_lines(&text, &path.display().to_string())
        }
    }
}

fn parse_ndjson_lines(text: &str, where_: &str) -> Result<Vec<Value>, DbsError> {
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| DbsError::Config(format!("{where_}:{lineno}: not valid JSON: {e}")))?;
        if !value.is_object() {
            return Err(DbsError::Config(format!(
                "{where_}:{lineno}: expected an object per line"
            )));
        }
        rows.push(value);
    }
    Ok(rows)
}

/// Maps one export row (the `row_to_item` shape) back to a
/// [`PreparedItem`] for the classified upsert path.
pub fn prepared_item_from_row(row: &Value, where_: &str) -> Result<PreparedItem, DbsError> {
    let external_id = row
        .get("external_id")
        .filter(|v| is_truthy(v))
        .map(display_value)
        .unwrap_or_default()
        .trim()
        .to_string();
    if external_id.is_empty() {
        return Err(DbsError::Config(format!(
            "{where_}: row has no external_id"
        )));
    }

    let content_hash = match row.get("content_hash").filter(|v| is_truthy(v)) {
        Some(v) => display_value(v),
        None => {
            return Err(DbsError::Config(format!(
                "{where_}: row has no content_hash"
            )))
        }
    };

    let raw = match row.get("raw").filter(|v| !v.is_null()) {
        Some(v) => v,
        None => {
            return Err(DbsError::Config(format!(
                "{where_}: row has no raw payload — this export was written with --no-raw and is not restore-grade; re-export without it."
            )));
        }
    };
    let raw_json = serde_json::to_string(raw)
        .map_err(|e| DbsError::Config(format!("{where_}: failed to encode raw payload: {e}")))?;

    let item_kind = row
        .get("item_kind")
        .filter(|v| is_truthy(v))
        .map(display_value)
        .unwrap_or_else(|| "item".to_string());
    let tags: Vec<String> = row
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(display_value).collect())
        .unwrap_or_default();
    let deleted = row.get("deleted").is_some_and(is_truthy);

    Ok(PreparedItem {
        external_id,
        item_kind,
        title: opt_display(row.get("title")),
        url: opt_display(row.get("url")),
        body: opt_display(row.get("body")),
        tags,
        item_created_at: opt_display(row.get("created_at")),
        item_updated_at: opt_display(row.get("updated_at")),
        content_hash,
        raw_json,
        deleted,
        media: Vec::new(),
    })
}

/// Result of [`verify_archive`]: a checksummed bundle's per-entry
/// sha256 verification. An empty `issues` list on a checksummed bundle
/// means every listed entry hashed clean *and* no unlisted entries are
/// present (an extra file smuggled into the zip is itself an integrity
/// failure). A pre-checksum bundle (older dbs) reports
/// `has_checksums: false` with no issues — there is nothing to verify
/// against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArchiveIntegrity {
    pub has_checksums: bool,
    pub verified: u64,
    pub issues: Vec<String>,
}

/// Checks a bundle's per-entry sha256 checksums (see the archive
/// exporter, #58).
pub fn verify_archive(path: &Path) -> Result<ArchiveIntegrity, DbsError> {
    let manifest = read_manifest(path)?.ok_or_else(|| {
        DbsError::Config(format!(
            "{} is not an archive bundle (bare ndjson has no manifest)",
            path.display()
        ))
    })?;
    let Some(checksums) = manifest.get("checksums").and_then(|v| v.as_object()) else {
        return Ok(ArchiveIntegrity {
            has_checksums: false,
            verified: 0,
            issues: Vec::new(),
        });
    };

    let file = std::fs::File::open(path).map_err(io_err)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    let names: std::collections::BTreeSet<String> =
        archive.file_names().map(|n| n.to_string()).collect();

    let mut sorted_checksums: Vec<(&String, &Value)> = checksums.iter().collect();
    sorted_checksums.sort_by(|a, b| a.0.cmp(b.0));

    let mut issues = Vec::new();
    let mut verified: u64 = 0;
    for (name, want) in &sorted_checksums {
        if !names.contains(*name) {
            issues.push(format!(
                "{name}: listed in the manifest but missing from the bundle"
            ));
            continue;
        }
        let mut entry = archive.by_name(name).map_err(zip_err)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 1 << 16];
        loop {
            let n = entry.read(&mut buf).map_err(io_err)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let digest = format!("{:x}", hasher.finalize());
        let want_str = want.as_str().unwrap_or_default();
        if digest != want_str {
            issues.push(format!(
                "{name}: sha256 mismatch (bundle is corrupt or was modified)"
            ));
        } else {
            verified += 1;
        }
    }

    let checksum_names: std::collections::BTreeSet<&String> = checksums.keys().collect();
    for name in &names {
        if name != "manifest.json" && !checksum_names.contains(name) {
            issues.push(format!(
                "{name}: present in the bundle but not listed in the manifest"
            ));
        }
    }

    Ok(ArchiveIntegrity {
        has_checksums: true,
        verified,
        issues,
    })
}

/// `(revision rows, media files)` present in the bundle but not
/// restored.
pub fn skipped_extras(manifest: Option<&Value>) -> (u64, u64) {
    let counts = manifest.and_then(|m| m.get("counts"));
    let revisions = counts
        .and_then(|c| c.get("revisions"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let media = counts
        .and_then(|c| c.get("media"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    (revisions, media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-restore-test-{label}-{}-{}",
            std::process::id(),
            std::ptr::addr_of!(label) as usize
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_ndjson_bundle(path: &std::path::Path, rows: &[Value]) {
        let mut file = std::fs::File::create(path).unwrap();
        for row in rows {
            writeln!(file, "{}", serde_json::to_string(row).unwrap()).unwrap();
        }
    }

    fn write_archive_bundle(
        path: &std::path::Path,
        items_by_source: &[(&str, Vec<Value>)],
        manifest_extra: Value,
    ) {
        let file = std::fs::File::create(path).unwrap();
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut zf = ZipWriter::new(file);
        let mut checksums = serde_json::Map::new();
        for (source, rows) in items_by_source {
            let mut text = String::new();
            for row in rows {
                text.push_str(&serde_json::to_string(row).unwrap());
                text.push('\n');
            }
            let name = format!("items/{source}.ndjson");
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            checksums.insert(name.clone(), json!(format!("{:x}", hasher.finalize())));
            zf.start_file(&name, options).unwrap();
            zf.write_all(text.as_bytes()).unwrap();
        }
        let mut manifest = manifest_extra;
        manifest["checksums"] = Value::Object(checksums);
        zf.start_file("manifest.json", options).unwrap();
        zf.write_all(serde_json::to_vec_pretty(&manifest).unwrap().as_slice())
            .unwrap();
        zf.finish().unwrap();
    }

    fn item_row(external_id: &str, source: &str) -> Value {
        json!({
            "source": source,
            "external_id": external_id,
            "item_kind": "bookmark",
            "title": "hello",
            "content_hash": "abc123",
            "raw": {"a": 1},
        })
    }

    #[test]
    fn read_manifest_returns_none_for_a_bare_ndjson_file() {
        let dir = temp_dir("bare-ndjson");
        let path = dir.join("export.ndjson");
        write_ndjson_bundle(&path, &[item_row("e1", "raindrop")]);
        assert_eq!(read_manifest(&path).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_manifest_errors_on_a_zip_with_no_manifest() {
        let dir = temp_dir("no-manifest");
        let path = dir.join("bundle.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut zf = ZipWriter::new(file);
        zf.start_file("items/raindrop.ndjson", SimpleFileOptions::default())
            .unwrap();
        zf.write_all(b"{}\n").unwrap();
        zf.finish().unwrap();

        let err = read_manifest(&path).unwrap_err();
        assert!(err.to_string().contains("no manifest.json"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iter_export_rows_reads_a_bare_ndjson_file() {
        let dir = temp_dir("iter-ndjson");
        let path = dir.join("export.ndjson");
        write_ndjson_bundle(
            &path,
            &[item_row("e1", "raindrop"), item_row("e2", "raindrop")],
        );
        let rows = iter_export_rows(&path).unwrap();
        assert_eq!(rows.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iter_export_rows_reads_every_items_file_in_an_archive() {
        let dir = temp_dir("iter-archive");
        let path = dir.join("bundle.zip");
        write_archive_bundle(
            &path,
            &[
                ("raindrop", vec![item_row("e1", "raindrop")]),
                (
                    "reddit",
                    vec![item_row("e2", "reddit"), item_row("e3", "reddit")],
                ),
            ],
            json!({"db_schema_version": 1}),
        );
        let rows = iter_export_rows(&path).unwrap();
        assert_eq!(rows.len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iter_export_rows_errors_on_invalid_json() {
        let dir = temp_dir("iter-bad-json");
        let path = dir.join("export.ndjson");
        std::fs::write(&path, "not json\n").unwrap();
        let err = iter_export_rows(&path).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prepared_item_from_row_maps_every_field() {
        let row = json!({
            "source": "raindrop",
            "external_id": " e1 ",
            "item_kind": "bookmark",
            "title": "Hello",
            "url": "https://example.com",
            "body": "body text",
            "tags": ["a", "b"],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-02T00:00:00Z",
            "content_hash": "abc123",
            "raw": {"x": 1},
            "deleted": false,
        });
        let item = prepared_item_from_row(&row, "test").unwrap();
        assert_eq!(item.external_id, "e1");
        assert_eq!(item.item_kind, "bookmark");
        assert_eq!(item.title, Some("Hello".to_string()));
        assert_eq!(item.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(item.content_hash, "abc123");
        assert_eq!(item.raw_json, r#"{"x":1}"#);
        assert!(!item.deleted);
    }

    #[test]
    fn prepared_item_from_row_errors_without_external_id() {
        let row = json!({"content_hash": "x", "raw": {}});
        let err = prepared_item_from_row(&row, "test").unwrap_err();
        assert!(err.to_string().contains("no external_id"));
    }

    #[test]
    fn prepared_item_from_row_errors_without_content_hash() {
        let row = json!({"external_id": "e1", "raw": {}});
        let err = prepared_item_from_row(&row, "test").unwrap_err();
        assert!(err.to_string().contains("no content_hash"));
    }

    #[test]
    fn prepared_item_from_row_errors_without_raw_payload() {
        let row = json!({"external_id": "e1", "content_hash": "x"});
        let err = prepared_item_from_row(&row, "test").unwrap_err();
        assert!(err.to_string().contains("--no-raw"));
    }

    #[test]
    fn verify_archive_reports_clean_on_a_matching_bundle() {
        let dir = temp_dir("verify-clean");
        let path = dir.join("bundle.zip");
        write_archive_bundle(
            &path,
            &[("raindrop", vec![item_row("e1", "raindrop")])],
            json!({"db_schema_version": 1}),
        );
        let integrity = verify_archive(&path).unwrap();
        assert!(integrity.has_checksums);
        assert_eq!(integrity.verified, 1);
        assert!(integrity.issues.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_archive_flags_a_tampered_entry() {
        let dir = temp_dir("verify-tampered");
        let path = dir.join("bundle.zip");
        write_archive_bundle(
            &path,
            &[("raindrop", vec![item_row("e1", "raindrop")])],
            json!({"db_schema_version": 1}),
        );

        // Tamper: rewrite the zip with the same checksums but different content.
        let bytes = std::fs::read(&path).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let manifest_text = {
            let mut e = archive.by_name("manifest.json").unwrap();
            let mut s = String::new();
            e.read_to_string(&mut s).unwrap();
            s
        };
        drop(archive);

        let file = std::fs::File::create(&path).unwrap();
        let mut zf = ZipWriter::new(file);
        zf.start_file("items/raindrop.ndjson", SimpleFileOptions::default())
            .unwrap();
        zf.write_all(b"tampered content\n").unwrap();
        zf.start_file("manifest.json", SimpleFileOptions::default())
            .unwrap();
        zf.write_all(manifest_text.as_bytes()).unwrap();
        zf.finish().unwrap();

        let integrity = verify_archive(&path).unwrap();
        assert_eq!(integrity.verified, 0);
        assert_eq!(integrity.issues.len(), 1);
        assert!(integrity.issues[0].contains("sha256 mismatch"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skipped_extras_reads_counts_from_the_manifest() {
        let manifest = json!({"counts": {"revisions": 5, "media": 2}});
        assert_eq!(skipped_extras(Some(&manifest)), (5, 2));
        assert_eq!(skipped_extras(None), (0, 0));
    }
}
