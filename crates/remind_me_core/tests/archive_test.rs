//! Raw-transcript retention (#212).
//!
//! The property under test is the one the feature exists for: after an import,
//! a memory can hand back the *envelope* it came from — including the
//! `tool_use` and `thinking` blocks `text_of` deliberately dropped on the way
//! in. Asserting only that "an archive file appeared" would pass with the
//! spans wired to the wrong lines, which is the failure worth catching.

use remind_me_core::archive::{self, ARCHIVE_DIR_ENV};
use remind_me_core::importer::import_chat;
use remind_me_core::undo_import::undo_import;
use remind_me_core::{
    ChatImportInput, Database, ImportKind, ImportOutcome, UndoImportInput, UndoImportKind,
};
use rusqlite::Connection;

/// `REMIND_ME_ARCHIVE_DIR` is process-global, so every test that sets it runs
/// serialized behind this. Poisoning is ignored: a panicking test has already
/// failed, and letting it block the rest converts one failure into all of them.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A scratch directory inside the default import root (the home directory),
/// matching `importer_test.rs`'s own helper — an import source has to sit
/// inside the import roots or containment refuses it.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_archive_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One Claude Code transcript line, with machine chatter around the text.
fn transcript_line(text: &str, thinking: &str, tool_input: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "uuid": "11111111-2222-3333-4444-555555555555",
        "parentUuid": "00000000-0000-0000-0000-000000000000",
        "sessionId": "sess_abc",
        "timestamp": "2026-08-10T12:00:00Z",
        "cwd": "/home/user/project",
        "message": {
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": thinking },
                { "type": "text", "text": text },
                { "type": "tool_use", "name": "Read", "input": { "file_path": tool_input } },
            ]
        }
    })
    .to_string()
}

fn import(conn: &Connection, path: &std::path::Path) -> ImportOutcome {
    import_chat(
        conn,
        &ChatImportInput {
            file_path: path.display().to_string(),
            category: "chat_import".into(),
            tags: vec![],
            extract_mode: "all".into(),
            max_length: 10_000,
            kind: ImportKind::Auto,
        },
    )
    .unwrap()
}

fn memory_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT id FROM memories ORDER BY chunk_index")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

fn open(dir: &std::path::Path) -> Database {
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

#[test]
fn retention_is_off_unless_the_directory_is_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(ARCHIVE_DIR_ENV);

    assert!(archive::archive_root().is_none());
    assert!(!archive::is_enabled());
}

#[test]
fn an_import_with_retention_off_records_no_spans() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(ARCHIVE_DIR_ENV);

    let dir = scratch("off");
    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "hmm", "/etc/hosts")).unwrap();

    let db = open(&dir);
    let conn = db.conn();
    import(&conn, &path);

    // The import still happened...
    assert!(!memory_ids(&conn).is_empty());
    // ...but nothing was retained, and the tables are present-and-empty rather
    // than missing: the read path must never have to tolerate absent tables.
    let spans: i64 = conn
        .query_row("SELECT count(*) FROM import_archive_spans", [], |r| {
            r.get(0)
        })
        .unwrap();
    let archives: i64 = conn
        .query_row("SELECT count(*) FROM import_archives", [], |r| r.get(0))
        .unwrap();
    assert_eq!((spans, archives), (0, 0));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_memory_can_recover_the_blocks_the_importer_dropped() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("roundtrip");
    let archive_dir = dir.join("archive");
    std::env::set_var(ARCHIVE_DIR_ENV, &archive_dir);

    let path = dir.join("chat.jsonl");
    std::fs::write(
        &path,
        transcript_line("the table is called memories", "let me check", "/etc/hosts"),
    )
    .unwrap();

    let db = open(&dir);
    let conn = db.conn();
    import(&conn, &path);

    let ids = memory_ids(&conn);
    assert_eq!(ids.len(), 1, "one text block, so one memory");

    // What the memory itself holds: the flattened text, nothing else.
    let stored: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = ?",
            [&ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "the table is called memories");
    assert!(!stored.contains("let me check"));

    // What the archive hands back: the whole envelope, including everything
    // `text_of` dropped and every field the importer never read.
    let source = archive::source_for(&conn, &ids[0], false).unwrap().unwrap();
    assert!(source.content.contains("let me check"), "thinking block");
    assert!(source.content.contains("tool_use"), "tool call");
    assert!(source.content.contains("sessionId"), "envelope metadata");
    assert!(source.content.contains("parentUuid"), "threading");
    assert!(!source.truncated);
    assert_eq!(source.filename, "chat.jsonl");

    std::env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn each_memory_points_at_its_own_line_not_the_whole_file() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("spans");
    std::env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            transcript_line("first answer", "first thought", "/one"),
            transcript_line("second answer", "second thought", "/two"),
        ),
    )
    .unwrap();

    let db = open(&dir);
    let conn = db.conn();
    import(&conn, &path);

    let ids = memory_ids(&conn);
    assert_eq!(ids.len(), 2);

    let first = archive::source_for(&conn, &ids[0], false).unwrap().unwrap();
    let second = archive::source_for(&conn, &ids[1], false).unwrap().unwrap();

    // The whole point: a span is one envelope, not the file. Returning the
    // entire transcript for every memory would satisfy "content contains the
    // thinking block" while making drill-down worthless.
    assert!(first.content.contains("first thought"));
    assert!(!first.content.contains("second thought"));
    assert!(second.content.contains("second thought"));
    assert!(!second.content.contains("first thought"));
    assert_ne!(
        (first.byte_start, first.byte_end),
        (second.byte_start, second.byte_end)
    );

    std::env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn undoing_an_import_takes_its_archive_with_it() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("undo");
    std::env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "a thought", "/one")).unwrap();

    let db = open(&dir);
    let conn = db.conn();
    let outcome = import(&conn, &path);
    let import_id = match outcome {
        ImportOutcome::Imported { import_id, .. } => import_id,
        other => panic!("expected an import, got {:?}", other),
    };

    let blob: String = conn
        .query_row(
            "SELECT archive_path FROM import_archives WHERE import_id = ?",
            [&import_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(std::path::Path::new(&blob).exists());

    undo_import(
        &conn,
        &UndoImportInput {
            import_kind: UndoImportKind::Chat,
            import_id: Some(import_id.clone()),
            dry_run: false,
            limit: 100,
        },
    )
    .unwrap();

    // `undo_import` drops the tracking row so the content is re-importable.
    // An archive left behind would be a blob nothing references, and its
    // spans would point at memories that no longer exist.
    let archives: i64 = conn
        .query_row(
            "SELECT count(*) FROM import_archives WHERE import_id = ?",
            [&import_id],
            |r| r.get(0),
        )
        .unwrap();
    let spans: i64 = conn
        .query_row(
            "SELECT count(*) FROM import_archive_spans WHERE import_id = ?",
            [&import_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((archives, spans), (0, 0), "no orphaned archive rows");
    assert!(
        !std::path::Path::new(&blob).exists(),
        "the blob itself should be gone too"
    );

    std::env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_line_does_not_shift_the_spans_after_it() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("malformed");
    std::env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    // The importer skips a bad line rather than failing. Offsets are counted
    // over `split_inclusive('\n')`, so the skip must not desynchronise them —
    // a `lines()`-based counter would drop the separator and slide every
    // subsequent span one byte left per line.
    let path = dir.join("chat.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\nnot json at all\n{}\n",
            transcript_line("before the break", "t1", "/one"),
            transcript_line("after the break", "t2", "/two"),
        ),
    )
    .unwrap();

    let db = open(&dir);
    let conn = db.conn();
    import(&conn, &path);

    let ids = memory_ids(&conn);
    assert_eq!(ids.len(), 2, "the bad line is skipped, the rest survive");

    let second = archive::source_for(&conn, &ids[1], false).unwrap().unwrap();
    assert!(second.content.contains("after the break"));
    assert!(!second.content.contains("not json at all"));
    assert!(!second.content.contains("before the break"));

    std::env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
