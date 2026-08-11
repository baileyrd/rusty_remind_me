//! Code-reference staleness (#260): a memory anchored to a file, and what
//! happens when that file changes underneath it.
//!
//! # What #260 actually asked for, and what it got wrong
//!
//! The issue proposed reusing `watcher.rs`'s scan to notice a changed file.
//! That mechanism cannot fire for the issue's own example — `.rs` is not in
//! `import_paths::SUPPORTED_SUFFIXES`, so the watcher never enumerates source
//! files at all, watched directory or not. `code_refs` instead stats a known
//! path directly, on demand, which needs no directory enumeration and no
//! extension allowlist. See `code_refs.rs`'s module doc for the full case.
//!
//! These tests are built around real files in a temp directory: the feature
//! is entirely about filesystem state, and a fixture that never touches disk
//! would not exercise the part that can actually be wrong.

use remind_me_core::code_refs::{
    configured_code_roots, detect_code_refs, stale_candidates, StaleReason, CODE_ROOTS_ENV,
};
use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::sync::Mutex;

/// `REMIND_ME_CODE_ROOTS` is process-global.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_coderefs_db_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

/// A fresh directory to act as a code root, with one real file in it.
struct Fixture {
    root: std::path::PathBuf,
    file: std::path::PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("rrm_coderefs_root_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("auth.rs");
        std::fs::write(&file, "fn login() {}\n").unwrap();
        Fixture { root, file }
    }

    fn set_env(&self) {
        std::env::set_var(CODE_ROOTS_ENV, self.root.display().to_string());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct EnvGuard;
impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(CODE_ROOTS_ENV);
    }
}

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "fact".to_string(),
            tags: Vec::new(),
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: Vec::new(),
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

#[test]
fn unconfigured_is_completely_inert() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(CODE_ROOTS_ENV);

    let fixture = Fixture::new("inert");
    assert!(configured_code_roots().is_empty());

    // A real, existing file, named exactly -- and still nothing detected,
    // because detect_code_refs must return before ever touching the
    // filesystem when no root is configured.
    let refs = detect_code_refs(&fixture.file.display().to_string());
    assert!(refs.is_empty());
}

#[test]
fn a_real_file_inside_a_configured_root_is_anchored() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("anchor");
    fixture.set_env();
    let _env = EnvGuard;

    let content = format!(
        "don't touch {} -- mobile still uses it",
        fixture.file.display()
    );
    let refs = detect_code_refs(&content);

    assert_eq!(refs.len(), 1);
    assert_eq!(
        refs[0].path,
        fixture.file.canonicalize().unwrap().display().to_string()
    );
}

#[test]
fn a_nonexistent_path_records_no_anchor() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("missing");
    fixture.set_env();
    let _env = EnvGuard;

    let ghost = fixture.root.join("does_not_exist.rs");
    let refs = detect_code_refs(&format!("see {}", ghost.display()));
    assert!(refs.is_empty(), "a path that never existed must not anchor");
}

#[test]
fn a_real_file_outside_the_configured_root_is_not_anchored() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("contained_root");
    let outside = Fixture::new("outside");
    fixture.set_env();
    let _env = EnvGuard;

    // outside.file exists on disk, but is not under fixture.root -- the
    // containment boundary must reject it even though the existence check
    // alone would accept it.
    let refs = detect_code_refs(&format!("see {}", outside.file.display()));
    assert!(refs.is_empty(), "existence is necessary but not sufficient");
}

#[test]
fn ordinary_prose_is_not_stat_against() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("prose");
    fixture.set_env();
    let _env = EnvGuard;

    // Plausible sentence, no path-shaped tokens with '.' or '/'. If this
    // somehow resolved to something, containment or existence would still
    // reject it, but the point is the cheap filter skips ordinary words
    // before ever reaching the filesystem.
    let refs = detect_code_refs("the plan is to ship on Friday without ceremony");
    assert!(refs.is_empty());
}

#[test]
fn the_same_path_mentioned_twice_anchors_once() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("dedup");
    fixture.set_env();
    let _env = EnvGuard;

    let content = format!(
        "see {} -- also {}",
        fixture.file.display(),
        fixture.file.display()
    );
    assert_eq!(detect_code_refs(&content).len(), 1);
}

#[test]
fn wrapping_punctuation_is_stripped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("wrapped");
    fixture.set_env();
    let _env = EnvGuard;

    let content = format!("check `{}` before merging", fixture.file.display());
    assert_eq!(detect_code_refs(&content).len(), 1);
}

#[test]
fn add_memory_anchors_when_configured() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("add_memory_on");
    fixture.set_env();
    let _env = EnvGuard;

    let db = db("add_on");
    let conn = db.conn();
    let content = format!("don't refactor {} yet", fixture.file.display());
    add(&conn, &content);

    let candidates = stale_candidates(&conn, 20).unwrap();
    // Nothing has changed yet, so nothing is stale -- but this proves the
    // anchor was recorded at all, since an unanchored memory could never
    // appear here regardless of file state.
    assert!(candidates.is_empty(), "unchanged file must not be reported");

    let recorded: String = conn
        .query_row("SELECT metadata FROM memories LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        recorded.contains("code_refs"),
        "metadata must record the anchor: {recorded}"
    );
}

#[test]
fn add_memory_records_nothing_when_unconfigured() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(CODE_ROOTS_ENV);
    let fixture = Fixture::new("add_memory_off");

    let db = db("add_off");
    let conn = db.conn();
    let content = format!("don't refactor {} yet", fixture.file.display());
    add(&conn, &content);

    let recorded: String = conn
        .query_row("SELECT metadata FROM memories LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        !recorded.contains("code_refs"),
        "unconfigured must record nothing at all: {recorded}"
    );
}

#[test]
fn modifying_the_file_surfaces_the_memory_as_modified() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("modified");
    fixture.set_env();
    let _env = EnvGuard;

    let db = db("modified");
    let conn = db.conn();
    let memory_id = add(&conn, &format!("see {}", fixture.file.display()));

    // Deliberately changes the file's *size*, not just its mtime -- the
    // signature is truncated to whole seconds, so a test that only touched
    // mtime could pass or fail depending on how much of the current second
    // was left when it ran. #257 hit exactly this class of flakiness once
    // already; changing size makes the outcome timing-independent.
    std::fs::write(&fixture.file, "fn login() {}\nfn logout() {}\n").unwrap();

    let candidates = stale_candidates(&conn, 20).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].memory_id, memory_id);
    assert_eq!(candidates[0].stale_refs.len(), 1);
    assert_eq!(candidates[0].stale_refs[0].reason, StaleReason::Modified);
}

#[test]
fn deleting_the_file_surfaces_the_memory_as_deleted() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("deleted");
    fixture.set_env();
    let _env = EnvGuard;

    let db = db("deleted");
    let conn = db.conn();
    add(&conn, &format!("see {}", fixture.file.display()));

    std::fs::remove_file(&fixture.file).unwrap();

    let candidates = stale_candidates(&conn, 20).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].stale_refs[0].reason, StaleReason::Deleted);
}

#[test]
fn a_stale_memory_is_flagged_not_touched() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("untouched");
    fixture.set_env();
    let _env = EnvGuard;

    let db = db("untouched");
    let conn = db.conn();
    let memory_id = add(&conn, &format!("see {}", fixture.file.display()));

    let vitality_before: f64 = conn
        .query_row(
            "SELECT vitality FROM memories WHERE id = ?",
            [&memory_id],
            |r| r.get(0),
        )
        .unwrap();

    std::fs::remove_file(&fixture.file).unwrap();

    let before = stale_candidates(&conn, 20).unwrap();
    assert_eq!(before.len(), 1);

    // The core design decision: reporting a stale anchor must not supersede,
    // decay, or otherwise mutate the memory. It should still read back
    // exactly as written and still be searchable. Vitality is compared
    // against its own value from before the call rather than a literal --
    // the seed value a fresh `fact`/`manual` memory gets is a property of
    // `vitality.rs`'s priors, not of this feature, and hardcoding it here
    // would make this test wrong the moment those priors are tuned.
    let row: (Option<String>, Option<String>, f64) = conn
        .query_row(
            "SELECT superseded_by, deleted_at, vitality FROM memories WHERE id = ?",
            [&memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, None, "stale_candidates must not supersede");
    assert_eq!(row.1, None, "stale_candidates must not delete");
    assert_eq!(
        row.2, vitality_before,
        "stale_candidates must not touch vitality"
    );

    let found = queries::search_memories_budgeted(
        &conn,
        &remind_me_core::MemorySearchInput {
            query: "auth".to_string(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(
        found.results.iter().any(|r| r.memory.id == memory_id),
        "a stale memory must remain a normal, findable memory"
    );

    // Calling it again, unchanged, must report the same thing -- read-only
    // means idempotent by construction, not by luck.
    let after = stale_candidates(&conn, 20).unwrap();
    assert_eq!(before.len(), after.len());
}

#[test]
fn limit_bounds_candidates_not_paths_checked() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new("limit");
    fixture.set_env();
    let _env = EnvGuard;

    let db = db("limit");
    let conn = db.conn();
    for i in 0..5 {
        add(&conn, &format!("memo {i}: see {}", fixture.file.display()));
    }
    std::fs::remove_file(&fixture.file).unwrap();

    let capped = stale_candidates(&conn, 2).unwrap();
    assert_eq!(capped.len(), 2);
}
