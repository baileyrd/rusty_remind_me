//! Coverage for the schema migration ladder.

use remind_me_core::db::migrations::SCHEMA_VERSION;
use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::path::PathBuf;

/// Tables the reference has at v19. `memories_vec` is excluded: it exists only
/// when the `sqlite-vec` extension is loaded.
const REFERENCE_TABLES: [&str; 21] = [
    "chat_imports",
    "dbs_imports",
    "embedding_meta",
    "entities",
    "entity_relations",
    "memories",
    "memories_fts",
    "memory_associations",
    "memory_entities",
    "memory_feedback",
    "memory_tags",
    "mempalace_imports",
    "sync_flags",
    "sync_log",
    "sync_outbox",
    "sync_sends",
    "vec_chunks",
    "wiki_fts",
    "wiki_links",
    "wiki_meta",
    "wiki_pages",
];

/// `memories` columns, in the order the reference's ladder produces them.
const REFERENCE_MEMORY_COLUMNS: [&str; 26] = [
    "id",
    "content",
    "category",
    "tags",
    "source",
    "metadata",
    "created_at",
    "updated_at",
    "capture_id",
    "node_id",
    "client",
    "accessed_at",
    "access_count",
    "decay_rate",
    "vitality",
    "base_weight",
    "status",
    "memory_type",
    "source_capture_id",
    "subject",
    "predicate",
    "object",
    "superseded_by",
    "doc_id",
    "chunk_index",
    "deleted_at",
];

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rmm_mig_{}_{}_{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let dir = PathBuf::from(dir.to_string_lossy().replace(['(', ')', ' '], ""));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("m.db"))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(p) = self.0.parent() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", table))
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn user_version(conn: &Connection) -> i32 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_fresh_database_has_every_reference_table() {
    let db = Database::open_in_memory().unwrap();
    let present = tables(&db.conn());

    for expected in REFERENCE_TABLES {
        assert!(
            present.iter().any(|t| t == expected),
            "missing table {}; have {:?}",
            expected,
            present
        );
    }
}

#[test]
fn a_fresh_database_has_every_reference_memories_column() {
    let db = Database::open_in_memory().unwrap();
    let present = columns(&db.conn(), "memories");

    for expected in REFERENCE_MEMORY_COLUMNS {
        assert!(
            present.iter().any(|c| c == expected),
            "missing memories.{}; have {:?}",
            expected,
            present
        );
    }
    assert!(
        !present.iter().any(|c| c == "last_accessed_at"),
        "last_accessed_at was renamed to accessed_at for parity; it must not linger"
    );
}

#[test]
fn the_version_stamp_matches_the_schema_that_is_actually_present() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(user_version(&db.conn()), SCHEMA_VERSION);
}

#[test]
fn migrating_is_idempotent_across_reopens() {
    let tmp = TempDb::new("idempotent");

    let first = {
        let db = Database::open(&tmp.0).unwrap();
        let conn = db.conn();
        (
            tables(&conn),
            columns(&conn, "memories"),
            user_version(&conn),
        )
    };
    let second = {
        let db = Database::open(&tmp.0).unwrap();
        let conn = db.conn();
        (
            tables(&conn),
            columns(&conn, "memories"),
            user_version(&conn),
        )
    };

    assert_eq!(first, second, "reopening must not change the schema");
}

#[test]
fn a_database_falsely_stamped_19_is_detected_and_repaired() {
    let tmp = TempDb::new("falsestamp");

    // Reproduce exactly what earlier versions of this crate wrote: the old
    // 7-table schema, with last_accessed_at, stamped 19.
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'general',
                tags TEXT NOT NULL DEFAULT '[]',
                source TEXT NOT NULL DEFAULT 'manual',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                capture_id TEXT, subject TEXT, predicate TEXT, object TEXT,
                superseded_by TEXT,
                decay_rate REAL NOT NULL DEFAULT 0.10,
                vitality REAL NOT NULL DEFAULT 1.0,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed_at TEXT NOT NULL DEFAULT (datetime('now')),
                deleted_at TEXT DEFAULT NULL
            );
            CREATE TABLE chat_imports (
                import_id TEXT PRIMARY KEY, filename TEXT NOT NULL,
                hash TEXT NOT NULL, imported_at TEXT NOT NULL,
                stats TEXT NOT NULL DEFAULT '{}'
            );
            CREATE VIRTUAL TABLE memories_fts USING fts5(
                content, category, tags, content='memories', content_rowid='rowid');
            INSERT INTO memories (id, content, created_at, updated_at, last_accessed_at)
            VALUES ('mem_pre', 'written before the ladder existed',
                    '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00',
                    '2026-01-01T00:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    // Opening it must notice the stamp is a lie and replay the ladder.
    let db = Database::open(&tmp.0).unwrap();
    let conn = db.conn();

    for expected in REFERENCE_TABLES {
        assert!(
            tables(&conn).iter().any(|t| t == expected),
            "repair did not create {}",
            expected
        );
    }
    assert_eq!(user_version(&conn), SCHEMA_VERSION);

    let cols = columns(&conn, "memories");
    assert!(cols.iter().any(|c| c == "accessed_at"));
    assert!(!cols.iter().any(|c| c == "last_accessed_at"));
}

#[test]
fn repair_renames_rather_than_discarding_access_times() {
    let tmp = TempDb::new("preserve");
    let original = "2020-06-15T12:00:00+00:00";

    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(&format!(
            "
            CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'general',
                tags TEXT NOT NULL DEFAULT '[]',
                source TEXT NOT NULL DEFAULT 'manual',
                metadata TEXT NOT NULL DEFAULT '{{}}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed_at TEXT NOT NULL
            );
            INSERT INTO memories (id, content, created_at, updated_at, last_accessed_at)
            VALUES ('mem_old', 'old', '{0}', '{0}', '{0}');
            PRAGMA user_version = 19;
            ",
            original
        ))
        .unwrap();
    }

    let db = Database::open(&tmp.0).unwrap();
    let kept: String = db
        .conn()
        .query_row(
            "SELECT accessed_at FROM memories WHERE id = 'mem_old'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(
        kept, original,
        "a rename preserves the value; adding a new column would have reset it"
    );
}

#[test]
fn existing_rows_survive_the_repair() {
    let tmp = TempDb::new("survive");
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'general',
                tags TEXT NOT NULL DEFAULT '[\"kept\"]',
                source TEXT NOT NULL DEFAULT 'manual',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            INSERT INTO memories (id, content, created_at, updated_at)
            VALUES ('mem_keep', 'survivor', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    let db = Database::open(&tmp.0).unwrap();
    let conn = db.conn();

    let content: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = 'mem_keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(content, "survivor");

    // The v2 backfill should have populated memory_tags from the JSON column.
    let tag_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_tags WHERE memory_id = 'mem_keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag_count, 1, "pre-existing JSON tags must be backfilled");
}

#[test]
fn the_tags_trigger_keeps_memory_tags_in_step() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let mem = queries::add_memory(
        &conn,
        MemoryAddInput {
            content: "tagged".into(),
            category: "general".into(),
            tags: vec!["rust".into(), "mcp".into()],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    let count = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT count(*) FROM memory_tags WHERE memory_id = ?",
            rusqlite::params![mem.id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(&conn), 2, "insert trigger populates memory_tags");

    queries::update_memory(
        &conn,
        &remind_me_core::MemoryUpdateInput {
            memory_id: mem.id.clone(),
            content: None,
            category: None,
            tags: Some(vec!["rust".into()]),
            metadata: None,
        },
    )
    .unwrap();
    assert_eq!(count(&conn), 1, "update trigger re-syncs memory_tags");

    queries::delete_memory(&conn, &mem.id).unwrap();
    assert_eq!(count(&conn), 0, "delete trigger clears memory_tags");
}

#[test]
fn writes_are_recorded_in_the_sync_outbox() {
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

    // Without these triggers a database written here would look fully migrated
    // to `remind_me` while never propagating anything.
    let (op, payload): (String, String) = conn
        .query_row(
            "SELECT operation, payload FROM sync_outbox ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(op, "insert");
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed["content"], "syncable");
    assert!(parsed.get("base_weight").is_some());
    assert!(
        parsed.get("deleted_at").is_none(),
        "payload stops at v7's columns, matching the reference exactly"
    );
}

#[test]
fn base_weight_is_stored_and_drives_effective_vitality() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // "decision" has a type prior of 1.3; "manual" a source prior of 1.0.
    let mem = queries::add_memory(
        &conn,
        MemoryAddInput {
            content: "important".into(),
            category: "decision".into(),
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

    assert!(
        (mem.base_weight - 1.3).abs() < 1e-9,
        "base_weight is a real column now, got {}",
        mem.base_weight
    );
    assert!(
        (remind_me_core::vitality::effective_vitality(&mem, chrono::Utc::now()) - 1.3).abs() < 1e-3,
        "a fresh memory's effective vitality equals its base weight"
    );
}
