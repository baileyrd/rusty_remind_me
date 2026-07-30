//! Schema parity and reconciliation.
//!
//! The earlier version of these tests checked table *names* and `memories`
//! columns. That let four tables diverge in their columns and constraints
//! unnoticed — the check was shaped like the mistake. These compare the whole
//! schema: every table, index and trigger, by normalised DDL.

use remind_me_core::db::migrations::SCHEMA_VERSION;
use remind_me_core::db::queries;
use remind_me_core::sync::{HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// `writes_still_reach_the_sync_outbox` is the only test here that touches
/// the sync env vars (`#76`'s `sync_flags` gate means a write only reaches
/// the outbox when sync is actually configured) — held for consistency with
/// every other file that touches them, not because another test in this file
/// currently races on it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The generated schema, as shipped. Comparing against these is comparing
/// against `remind_me`, because they are dumped from it verbatim.
const SCHEMA_TABLES: &str = include_str!("../src/db/schema_tables.sql");
const SCHEMA_INDEXES: &str = include_str!("../src/db/schema_indexes.sql");
const SCHEMA_TRIGGERS: &str = include_str!("../src/db/schema_triggers.sql");

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(
            format!(
                "rmm_schema_{}_{}_{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            )
            .replace(['(', ')', ' '], ""),
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("s.db"))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(p) = self.0.parent() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// Collapse whitespace, strip comments and `IF NOT EXISTS`, lowercase — so two
/// DDL strings compare equal when they describe the same object.
fn normalise(sql: &str) -> String {
    let no_comments: String = sql
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    no_comments
        .replace("IF NOT EXISTS ", "")
        // ALTER TABLE ... RENAME stores the new name quoted, so a rebuilt table
        // reads back as CREATE TABLE "memories". Same object, different text.
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Every object of `kind`, keyed by name, with normalised DDL. Excludes the
/// shadow tables FTS5 manages itself.
fn objects(conn: &Connection, kind: &str) -> BTreeMap<String, String> {
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = ? AND sql IS NOT NULL")
        .unwrap();
    stmt.query_map([kind], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .filter(|(name, _)| {
        !name.starts_with("sqlite_")
            && !["_config", "_data", "_docsize", "_idx", "_content"]
                .iter()
                .any(|s| name.ends_with(s))
    })
    .map(|(name, sql)| (name, normalise(&sql)))
    .collect()
}

/// Compare live objects of `kind` against the shipped schema, reporting only
/// what differs. A whole-map `assert_eq!` dumps twenty tables of DDL and buries
/// the one that is wrong.
fn assert_matches_schema(live: &Connection, kind: &str) {
    let actual = objects(live, kind);
    let want = objects(&expected(), kind);

    let mut problems = Vec::new();
    for (name, want_sql) in &want {
        match actual.get(name) {
            None => problems.push(format!("  MISSING {} {}", kind, name)),
            Some(got) if got != want_sql => problems.push(format!(
                "  {} {} DIFFERS\n    want: {}\n    got:  {}",
                kind, name, want_sql, got
            )),
            _ => {}
        }
    }
    for name in actual.keys() {
        if !want.contains_key(name) {
            problems.push(format!("  UNEXPECTED {} {}", kind, name));
        }
    }

    assert!(
        problems.is_empty(),
        "schema does not match the generated SQL:\n{}",
        problems.join("\n")
    );
}

/// A database holding exactly the shipped schema, built independently of the
/// code under test.
fn expected() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_TABLES).unwrap();
    conn.execute_batch(SCHEMA_INDEXES).unwrap();
    conn.execute_batch(SCHEMA_TRIGGERS).unwrap();
    conn
}

#[test]
fn every_table_matches_the_generated_schema() {
    let db = Database::open_in_memory().unwrap();
    assert_matches_schema(&db.conn(), "table");
}

#[test]
fn every_index_matches_the_generated_schema() {
    let db = Database::open_in_memory().unwrap();
    assert_matches_schema(&db.conn(), "index");
}

#[test]
fn every_trigger_matches_the_generated_schema() {
    let db = Database::open_in_memory().unwrap();
    assert_matches_schema(&db.conn(), "trigger");
}

#[test]
fn the_schema_carries_no_target_only_columns() {
    // The four tables that had drifted. Each assertion names a column that was
    // present here and absent upstream, or vice versa.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let cols = |t: &str| -> Vec<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", t)).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    let wiki = cols("wiki_pages");
    assert!(wiki.contains(&"summary".to_string()));
    assert!(wiki.contains(&"mtime".to_string()));
    assert!(
        !wiki.contains(&"topic".to_string()),
        "topic was target-only"
    );

    assert!(cols("entities").contains(&"node_id".to_string()));
    assert!(cols("memory_entities").contains(&"created_at".to_string()));

    let relations = cols("entity_relations");
    assert!(relations.contains(&"subject_entity_id".to_string()));
    assert!(relations.contains(&"relation".to_string()));
    assert!(
        !relations.contains(&"subject_id".to_string()),
        "subject_id was this crate's own name for it"
    );
}

#[test]
fn memory_entities_has_no_foreign_keys() {
    // The reference omits them deliberately: sync can deliver a mention link
    // before the memory it points at, and a cascade would reject that.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_list(memory_entities)")
        .unwrap();
    let count = stmt.query_map([], |_| Ok(())).unwrap().count();
    assert_eq!(count, 0);
}

#[test]
fn the_version_stamp_matches_the_schema_present() {
    let db = Database::open_in_memory().unwrap();
    let version: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn reopening_changes_nothing() {
    let tmp = TempDb::new("idempotent");
    let snapshot = |path: &PathBuf| {
        let db = Database::open(path).unwrap();
        let conn = db.conn();
        (
            objects(&conn, "table"),
            objects(&conn, "index"),
            objects(&conn, "trigger"),
        )
    };
    assert_eq!(snapshot(&tmp.0), snapshot(&tmp.0));
}

#[test]
fn a_legacy_database_is_reconciled_to_the_generated_schema() {
    let tmp = TempDb::new("legacy");

    // Exactly what earlier versions of this crate wrote: the old shapes, with
    // last_accessed_at, wiki_pages.topic, cascading memory_entities, stamped 19.
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'general',
                tags TEXT NOT NULL DEFAULT '[\"carried\"]',
                source TEXT NOT NULL DEFAULT 'manual',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed_at TEXT NOT NULL
            );
            CREATE TABLE wiki_pages (
                slug TEXT PRIMARY KEY, title TEXT NOT NULL, content TEXT NOT NULL,
                topic TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE entities (
                id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, kind TEXT,
                aliases TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE memory_entities (
                memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
                PRIMARY KEY (memory_id, entity_id)
            );
            INSERT INTO memories (id, content, created_at, updated_at, last_accessed_at)
            VALUES ('mem_old', 'survivor', '2020-06-15T12:00:00+00:00',
                    '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            INSERT INTO wiki_pages VALUES
                ('page', 'Page', 'body', 'general',
                 '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    let db = Database::open(&tmp.0).unwrap();
    let conn = db.conn();

    assert_matches_schema(&conn, "table");
    assert_matches_schema(&conn, "index");
    assert_matches_schema(&conn, "trigger");

    // Data survives the rebuild.
    let content: String = conn
        .query_row("SELECT content FROM memories WHERE id='mem_old'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(content, "survivor");

    let title: String = conn
        .query_row("SELECT title FROM wiki_pages WHERE slug='page'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        title, "Page",
        "the wiki_pages rebuild must carry rows across"
    );

    // The rename preserves the value rather than resetting it.
    let accessed: String = conn
        .query_row(
            "SELECT accessed_at FROM memories WHERE id='mem_old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(accessed, "2020-06-15T12:00:00+00:00");

    // Derived tables are backfilled for rows that predate the triggers.
    let tags: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_tags WHERE memory_id='mem_old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tags, 1, "pre-existing JSON tags must be backfilled");
}

#[test]
fn writes_still_reach_the_sync_outbox() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var(NODE_ID_ENV, "node-a");
    std::env::set_var(HUB_URL_ENV, "http://hub.example");
    std::env::set_var(SYNC_SECRET_ENV, "shh");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    queries::add_memory(
        &conn,
        MemoryAddInput {
            content: "syncable".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    let payload: String = conn
        .query_row(
            "SELECT payload FROM sync_outbox ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed["content"], "syncable");
    assert!(parsed.get("base_weight").is_some());

    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);
}

#[test]
fn writes_do_not_reach_the_outbox_while_sync_is_unconfigured() {
    // The `#76` regression case: memories_outbox_ai/au are gated on
    // sync_flags.sync_enabled, so a write on a node that has never
    // configured sync must not queue anything at all.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    queries::add_memory(
        &conn,
        MemoryAddInput {
            content: "not synced anywhere".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}
