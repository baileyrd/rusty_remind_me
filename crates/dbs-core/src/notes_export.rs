//! Unzipped Obsidian-notes/wiki export for folder-watching downstream
//! consumers.
//!
//! Mirrors `src/dbs/notes_export.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). `dbs export --format obsidian`/`wiki` each
//! produce a single zip; some consumers instead want a plain directory
//! of loose files — e.g. a folder watcher that only recognizes files,
//! not archives. [`export_notes`]/[`export_wiki_dir`] reuse
//! [`BackupService::export`]'s existing tested zip path (atomic write,
//! same frontmatter/media/manifest logic, pulled forward from #70 in
//! #61) and unpack the entries a watcher actually wants.
//!
//! [`export_notes`] is incremental by default: a JSON state file at
//! `<out_dir>/.dbs_export_state.json` records the wall-clock time the
//! previous successful call *started*, and passes it as the cutoff on
//! the next call so a scheduled `backup && export-notes` run only
//! writes new items, not the entire history every time. Recording the
//! start time (not completion) means an item created mid-run is never
//! permanently skipped — worst case it's picked up again next run,
//! which is safe because unchanged notes are byte-identical.
//!
//! The cutoff is applied as *created-or-updated*, since [`ExportQuery`]
//! itself only ANDs filters together: a plain `since` query would miss
//! an item created long ago but edited after the cutoff, leaving its
//! note stale forever. So when a cutoff is in effect, [`export_notes`]
//! issues two queries — one on `since`, one on `since_updated` — and
//! unions their results by `(source, external_id)` identity before
//! writing, so an item matching either one gets (re-)written exactly
//! once.
//!
//! Filename stability across runs: the obsidian exporter only
//! disambiguates title-slug collisions *within* one zip (a fresh
//! `seen_names` set per call), which is not enough here — two
//! different incremental runs could each independently pick the same
//! slug for two different items and silently overwrite one item's note
//! with another's. The state file also carries a persistent
//! `(source, external_id) -> filename` map so the same item always
//! lands in the same file (surviving title edits too), and a genuine
//! new collision is disambiguated the same way the exporter itself
//! would.
//!
//! [`export_wiki_dir`] is deliberately **not** incremental, unlike
//! [`export_notes`]: a `topic` page is an aggregate, not "new items
//! since the cutoff" — writing only the new ones would produce a hub
//! page that silently lost its history each run. So the full page set
//! is rebuilt every call, safe to repeat since pages are keyed by slug
//! and overwritten in place.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::errors::DbsError;
use crate::export::ExportResult;
use crate::service::BackupService;
use crate::storage::ExportQuery;
use crate::timeutil::{iso_z, parse_iso};

pub const STATE_FILENAME: &str = ".dbs_export_state.json";

fn io_err(e: std::io::Error) -> DbsError {
    DbsError::Storage(format!("notes export I/O failed: {e}"))
}

fn zip_err(e: zip::result::ZipError) -> DbsError {
    DbsError::Storage(format!("notes export failed to read its own zip: {e}"))
}

/// Pulls `(dbs_source, dbs_external_id)` back out of a rendered note —
/// matches the exact double-quoted-scalar rendering the obsidian
/// exporter's `yaml_scalar` produces for these two frontmatter fields.
fn parse_identity(note_text: &str) -> (Option<String>, Option<String>) {
    let mut source = None;
    let mut external_id = None;
    for line in note_text.lines() {
        if let Some(rest) = line.strip_prefix("dbs_source: \"") {
            source = extract_yaml_quoted(rest);
        } else if let Some(rest) = line.strip_prefix("dbs_external_id: \"") {
            external_id = extract_yaml_quoted(rest);
        }
    }
    (source, external_id)
}

/// `rest` is the text immediately after the opening quote; returns the
/// unescaped content up to (not including) the closing unescaped quote,
/// or `None` if the string never closes.
fn extract_yaml_quoted(rest: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

fn load_state(out_dir: &Path) -> Map<String, Value> {
    let path = out_dir.join(STATE_FILENAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => Map::new(),
    }
}

fn save_state(out_dir: &Path, state: &Map<String, Value>) -> Result<(), DbsError> {
    let path = out_dir.join(STATE_FILENAME);
    let tmp = out_dir.join(format!("{STATE_FILENAME}.tmp"));
    // `serde_json::Map` is `BTreeMap`-backed by default (no
    // `preserve_order` feature), so `to_string_pretty` already emits
    // sorted keys — matches the reference's `sort_keys=True`.
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| DbsError::Storage(format!("failed to encode export state: {e}")))?;
    std::fs::write(&tmp, text).map_err(io_err)?;
    std::fs::rename(&tmp, &path).map_err(io_err)?;
    Ok(())
}

/// Picks this run's on-disk filename for one note, stable across runs.
///
/// A known identity always reuses its previously assigned filename. A
/// new identity takes the exporter's own slug unless another identity
/// already holds it (this run or a prior one), in which case it
/// disambiguates with the external_id — mirroring the obsidian
/// exporter's own within-zip fallback, just applied across runs
/// instead of within one.
fn resolve_filename(
    identity_key: &str,
    zip_basename: &str,
    external_id: Option<&str>,
    filenames: &HashMap<String, String>,
    taken: &HashSet<String>,
) -> String {
    if let Some(existing) = filenames.get(identity_key) {
        return existing.clone();
    }
    if taken.contains(zip_basename) {
        let stem = zip_basename.strip_suffix(".md").unwrap_or(zip_basename);
        format!("{stem}-{}.md", external_id.unwrap_or("item"))
    } else {
        zip_basename.to_string()
    }
}

fn write_atomically(dest: &Path, bytes: &[u8]) -> Result<(), DbsError> {
    let mut tmp_name = dest.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = dest.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes).map_err(io_err)?;
    std::fs::rename(&tmp, dest).map_err(io_err)?;
    Ok(())
}

fn temp_zip_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "dbs-{label}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ))
}

/// Writes one Markdown note per live item into `out_dir` (unzipped).
///
/// `since`, when given, is an explicit lower bound on `item_created_at`
/// that overrides the incremental state file for this call. Otherwise,
/// with `incremental` set, resumes from the state file's last
/// successful run instead of exporting every live item.
///
/// Returns an [`ExportResult`] with `item_count` = notes written,
/// `format = "obsidian-notes"`, `path = Some(out_dir)`, and `extra`
/// carrying `since` (the effective cutoff ISO string, if any).
pub fn export_notes(
    service: &BackupService,
    out_dir: &Path,
    sources: Option<&[String]>,
    item_types: Option<&[String]>,
    since: Option<DateTime<Utc>>,
    incremental: bool,
) -> Result<ExportResult, DbsError> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| DbsError::Storage(format!("failed to create export directory: {e}")))?;
    let state = load_state(out_dir);
    let mut filenames: HashMap<String, String> = state
        .get("filenames")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let effective_since = since.or_else(|| {
        if incremental {
            parse_iso(state.get("last_export").and_then(|v| v.as_str()))
        } else {
            None
        }
    });

    let run_start = Utc::now();
    let mut queries = vec![ExportQuery {
        sources: sources.map(|s| s.to_vec()),
        item_types: item_types.map(|s| s.to_vec()),
        since: effective_since,
        include_deleted: false,
        include_revisions: false,
        include_raw: false,
        ..ExportQuery::default()
    }];
    if effective_since.is_some() {
        queries.push(ExportQuery {
            sources: sources.map(|s| s.to_vec()),
            item_types: item_types.map(|s| s.to_vec()),
            since_updated: effective_since,
            include_deleted: false,
            include_revisions: false,
            include_raw: false,
            ..ExportQuery::default()
        });
    }

    // identity_key -> (note text, zip's own basename, external_id). A
    // map so an item matched by both queries is written exactly once.
    let mut notes_by_identity: HashMap<String, (String, String, Option<String>)> = HashMap::new();
    for query in &queries {
        let tmp_zip = temp_zip_path("notes-export");
        let write_result = service.export(query, "obsidian", &tmp_zip, None);
        let read_result = write_result.and_then(|_| {
            let file = std::fs::File::open(&tmp_zip).map_err(io_err)?;
            let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
            let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
            for name in names {
                if !(name.starts_with("notes/") && name.ends_with(".md")) {
                    continue; // media/ and manifest.json aren't notes
                }
                let mut entry = archive.by_name(&name).map_err(zip_err)?;
                let mut text = String::new();
                entry.read_to_string(&mut text).map_err(io_err)?;
                drop(entry);
                let (source_name, external_id) = parse_identity(&text);
                let identity_key = format!(
                    "{}|{}",
                    source_name.as_deref().unwrap_or("None"),
                    external_id.as_deref().unwrap_or("None")
                );
                let zip_basename = name.rsplit('/').next().unwrap_or(name.as_str()).to_string();
                notes_by_identity.insert(identity_key, (text, zip_basename, external_id));
            }
            Ok(())
        });
        let _ = std::fs::remove_file(&tmp_zip);
        read_result?;
    }

    let mut written: u64 = 0;
    let mut taken: HashSet<String> = filenames.values().cloned().collect();
    for (identity_key, (text, zip_basename, external_id)) in &notes_by_identity {
        let filename = resolve_filename(
            identity_key,
            zip_basename,
            external_id.as_deref(),
            &filenames,
            &taken,
        );
        filenames.insert(identity_key.clone(), filename.clone());
        taken.insert(filename.clone());
        write_atomically(&out_dir.join(&filename), text.as_bytes())?;
        written += 1;
    }

    let mut new_state = Map::new();
    new_state.insert("last_export".to_string(), Value::String(iso_z(run_start)));
    new_state.insert(
        "filenames".to_string(),
        Value::Object(
            filenames
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        ),
    );
    save_state(out_dir, &new_state)?;

    let mut extra = HashMap::new();
    extra.insert(
        "since".to_string(),
        effective_since
            .map(iso_z)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );

    Ok(ExportResult {
        format: "obsidian-notes".to_string(),
        item_count: written,
        path: Some(out_dir.display().to_string()),
        extra,
        ..Default::default()
    })
}

/// Writes the `wiki` export's pages loose into `out_dir` (unzipped).
///
/// `index.md` is extracted alongside `pages/*.md` here — unlike
/// [`export_notes`], which drops the manifest so a watcher scanning
/// for `.md`/`.json` doesn't pick it up — because the index *is* wiki
/// content and carries the `[[wikilinks]]` tying the page set together.
/// `manifest.json` is still left behind.
///
/// Returns an [`ExportResult`] with `item_count` = items exported,
/// `format = "wiki-dir"`, `path = Some(out_dir)`, and `extra` carrying
/// `pages` (wiki pages, matching the `wiki` exporter's own count),
/// `files` (`pages` plus the index), and `grouping`.
pub fn export_wiki_dir(
    service: &BackupService,
    out_dir: &Path,
    sources: Option<&[String]>,
    item_types: Option<&[String]>,
    since: Option<DateTime<Utc>>,
    grouping: &str,
) -> Result<ExportResult, DbsError> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| DbsError::Storage(format!("failed to create export directory: {e}")))?;

    let query = ExportQuery {
        sources: sources.map(|s| s.to_vec()),
        item_types: item_types.map(|s| s.to_vec()),
        since,
        include_deleted: false,
        include_revisions: false,
        // Raw payloads are REQUIRED here, unlike export_notes: a
        // source's ExportProfile names its grouping axes and body
        // fields as raw paths, and with raw omitted none of them
        // resolve, so every source would silently fall back to
        // generic tag grouping.
        include_raw: true,
        wiki_grouping: grouping.to_string(),
        ..ExportQuery::default()
    };

    let tmp_zip = temp_zip_path("wiki-dir");
    let outcome = (|| -> Result<(ExportResult, u64, u64), DbsError> {
        let export_result = service.export(&query, "wiki", &tmp_zip, None)?;
        let (pages, files) = unpack_wiki_zip(&tmp_zip, out_dir)?;
        Ok((export_result, pages, files))
    })();
    let _ = std::fs::remove_file(&tmp_zip);
    let (result, pages, files) = outcome?;

    let mut extra = HashMap::new();
    extra.insert("pages".to_string(), Value::from(pages));
    extra.insert("files".to_string(), Value::from(files));
    extra.insert("grouping".to_string(), Value::String(grouping.to_string()));
    extra.insert(
        "since".to_string(),
        since.map(iso_z).map(Value::String).unwrap_or(Value::Null),
    );

    Ok(ExportResult {
        format: "wiki-dir".to_string(),
        item_count: result.item_count,
        path: Some(out_dir.display().to_string()),
        extra,
        ..Default::default()
    })
}

fn unpack_wiki_zip(tmp_zip: &Path, out_dir: &Path) -> Result<(u64, u64), DbsError> {
    let file = std::fs::File::open(tmp_zip).map_err(io_err)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();

    let mut pages: u64 = 0;
    let mut files: u64 = 0;
    for name in names {
        let is_page = name.starts_with("pages/") && name.ends_with(".md");
        if !(is_page || name == "index.md") {
            continue; // manifest.json isn't wiki content
        }
        let mut entry = archive.by_name(&name).map_err(zip_err)?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(io_err)?;
        drop(entry);
        let basename = name.rsplit('/').next().unwrap_or(name.as_str());
        write_atomically(&out_dir.join(basename), &bytes)?;
        files += 1;
        if is_page {
            pages += 1;
        }
    }
    Ok((pages, files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, NotifyOn, VpnGuard};
    use crate::registry::ConnectorRegistry;
    use crate::service::{BackupService, UnimplementedRunner};
    use crate::storage::sqlite_storage::SqliteStorage;
    use crate::storage::{BatchResult, PreparedItem, Storage};

    fn open_storage() -> SqliteStorage {
        let mut storage = SqliteStorage::open(":memory:").unwrap();
        storage.migrate().unwrap();
        storage
    }

    fn seed_item(
        storage: &mut SqliteStorage,
        source: &str,
        external_id: &str,
        title: &str,
        content_hash: &str,
    ) {
        let existing = storage
            .upsert_source(source, "raindrop", "raindrop", "{}", 1)
            .unwrap();
        let run_id = storage
            .begin_run(existing.id, "raindrop", "full", None)
            .unwrap();
        let item = PreparedItem {
            external_id: external_id.to_string(),
            item_kind: "bookmark".to_string(),
            title: Some(title.to_string()),
            url: Some(format!("https://example.com/{external_id}")),
            body: Some("body text".to_string()),
            tags: vec![],
            item_created_at: Some(iso_z(Utc::now())),
            item_updated_at: Some(iso_z(Utc::now())),
            content_hash: content_hash.to_string(),
            raw_json: "{}".to_string(),
            deleted: false,
            media: Vec::new(),
        };
        storage
            .upsert_items(existing.id, run_id, &[item], false, 0)
            .unwrap();
        storage
            .finish_run(
                run_id,
                "success",
                &BatchResult::default(),
                1,
                None,
                None,
                &[],
            )
            .unwrap();
    }

    fn test_config() -> Config {
        Config {
            database: ":memory:".to_string(),
            export_dir: String::new(),
            download_root: String::new(),
            default_overlap_seconds: 0,
            vpn_exec: String::new(),
            vpn_status: String::new(),
            vpn_netns: String::new(),
            vpn_guard: VpnGuard::Skip,
            notify_url: None,
            notify_on: NotifyOn::default(),
            http_timeout: 30.0,
            http_rate_limit_per_min: 0,
            batch_max: 0,
            sweep_safety_fraction: 0.5,
            parallel: 1,
            sources: HashMap::new(),
            connectors: HashMap::new(),
            base_dir: std::path::PathBuf::new(),
            source_path: None,
        }
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-notes-export-test-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_identity_extracts_source_and_external_id() {
        let text = "---\ndbs_source: \"raindrop\"\ndbs_external_id: \"e1\"\n---\n";
        assert_eq!(
            parse_identity(text),
            (Some("raindrop".to_string()), Some("e1".to_string()))
        );
    }

    #[test]
    fn parse_identity_unescapes_backslashes_and_quotes() {
        let text = "dbs_source: \"a\\\"b\\\\c\"\ndbs_external_id: \"e1\"\n";
        assert_eq!(
            parse_identity(text),
            (Some("a\"b\\c".to_string()), Some("e1".to_string()))
        );
    }

    #[test]
    fn resolve_filename_reuses_a_known_identitys_filename() {
        let mut filenames = HashMap::new();
        filenames.insert("raindrop|e1".to_string(), "old-name.md".to_string());
        let taken: HashSet<String> = HashSet::new();
        let name = resolve_filename("raindrop|e1", "new-slug.md", Some("e1"), &filenames, &taken);
        assert_eq!(name, "old-name.md");
    }

    #[test]
    fn resolve_filename_disambiguates_a_cross_run_collision() {
        let filenames: HashMap<String, String> = HashMap::new();
        let mut taken = HashSet::new();
        taken.insert("same.md".to_string());
        let name = resolve_filename("raindrop|e2", "same.md", Some("e2"), &filenames, &taken);
        assert_eq!(name, "same-e2.md");
    }

    #[test]
    fn export_notes_first_run_writes_every_live_item() {
        let mut storage = open_storage();
        seed_item(&mut storage, "raindrop", "e1", "First", "h1");
        seed_item(&mut storage, "raindrop", "e2", "Second", "h2");

        let config = test_config();
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = UnimplementedRunner;
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let dir = temp_dir("first-run");
        let result = export_notes(&service, &dir, None, None, None, true).unwrap();
        assert_eq!(result.item_count, 2);
        assert_eq!(result.format, "obsidian-notes");
        assert!(dir.join("First.md").exists());
        assert!(dir.join("Second.md").exists());
        assert!(dir.join(STATE_FILENAME).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_notes_incremental_rerun_only_writes_new_items() {
        let mut storage = open_storage();
        seed_item(&mut storage, "raindrop", "e1", "First", "h1");

        let config = test_config();
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = UnimplementedRunner;
        let dir = temp_dir("incremental");

        {
            let service = BackupService::new(&mut storage, &config, &registry, &runner);
            let result = export_notes(&service, &dir, None, None, None, true).unwrap();
            assert_eq!(result.item_count, 1);
        }

        // A little real wall-clock separation so the second item's
        // created_at lands after the first run's recorded cutoff.
        std::thread::sleep(std::time::Duration::from_millis(20));
        seed_item(&mut storage, "raindrop", "e2", "Second", "h2");

        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        let result = export_notes(&service, &dir, None, None, None, true).unwrap();
        assert_eq!(result.item_count, 1);
        assert!(dir.join("Second.md").exists());
        // The first run's note is untouched, not rewritten.
        assert!(dir.join("First.md").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_notes_disambiguates_a_same_titled_item_across_runs() {
        let mut storage = open_storage();
        seed_item(&mut storage, "raindrop", "e1", "Same Title", "h1");

        let config = test_config();
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = UnimplementedRunner;
        let dir = temp_dir("collision");

        {
            let service = BackupService::new(&mut storage, &config, &registry, &runner);
            export_notes(&service, &dir, None, None, None, true).unwrap();
        }
        assert!(dir.join("Same_Title.md").exists());

        std::thread::sleep(std::time::Duration::from_millis(20));
        seed_item(&mut storage, "raindrop", "e2", "Same Title", "h2");

        let service = BackupService::new(&mut storage, &config, &registry, &runner);
        export_notes(&service, &dir, None, None, None, true).unwrap();

        // Item 1 keeps its original file; item 2's collision is
        // disambiguated by external_id, matching the reference.
        assert!(dir.join("Same_Title.md").exists());
        assert!(dir.join("Same_Title-e2.md").exists());
        let first_text = std::fs::read_to_string(dir.join("Same_Title.md")).unwrap();
        assert!(first_text.contains("dbs_external_id: \"e1\""));
        let second_text = std::fs::read_to_string(dir.join("Same_Title-e2.md")).unwrap();
        assert!(second_text.contains("dbs_external_id: \"e2\""));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_wiki_dir_writes_pages_and_index_but_not_the_manifest() {
        let mut storage = open_storage();
        seed_item(&mut storage, "raindrop", "e1", "Hello", "h1");

        let config = test_config();
        let registry = ConnectorRegistry::from_resolved([]);
        let runner = UnimplementedRunner;
        let service = BackupService::new(&mut storage, &config, &registry, &runner);

        let dir = temp_dir("wiki-dir");
        let result = export_wiki_dir(&service, &dir, None, None, None, "topic").unwrap();
        assert_eq!(result.format, "wiki-dir");
        assert!(dir.join("index.md").exists());
        assert!(!dir.join("manifest.json").exists());
        assert!(result.extra.contains_key("pages"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
