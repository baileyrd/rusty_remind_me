use rusqlite::{Connection, Result};

pub const SCHEMA_VERSION: i32 = 19;

pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA busy_timeout=5000;
        PRAGMA foreign_keys=ON;

        CREATE TABLE IF NOT EXISTS memories (
            id          TEXT PRIMARY KEY,
            content     TEXT NOT NULL,
            category    TEXT NOT NULL DEFAULT 'general',
            tags        TEXT NOT NULL DEFAULT '[]',
            source      TEXT NOT NULL DEFAULT 'manual',
            metadata    TEXT NOT NULL DEFAULT '{}',
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            capture_id  TEXT DEFAULT NULL,
            subject     TEXT DEFAULT NULL,
            predicate   TEXT DEFAULT NULL,
            object      TEXT DEFAULT NULL,
            superseded_by TEXT DEFAULT NULL,
            decay_rate  REAL NOT NULL DEFAULT 0.10,
            vitality    REAL NOT NULL DEFAULT 1.0,
            access_count INTEGER NOT NULL DEFAULT 0,
            last_accessed_at TEXT NOT NULL DEFAULT (datetime('now')),
            deleted_at TEXT DEFAULT NULL
        );

        CREATE TABLE IF NOT EXISTS chat_imports (
            import_id   TEXT PRIMARY KEY,
            filename    TEXT NOT NULL,
            hash        TEXT NOT NULL,
            imported_at TEXT NOT NULL,
            stats       TEXT NOT NULL DEFAULT '{}'
        );

        CREATE TABLE IF NOT EXISTS entities (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            kind        TEXT DEFAULT NULL,
            aliases     TEXT NOT NULL DEFAULT '[]',
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS memory_entities (
            memory_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            entity_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            PRIMARY KEY (memory_id, entity_id)
        );

        CREATE TABLE IF NOT EXISTS entity_relations (
            id          TEXT PRIMARY KEY,
            subject_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            predicate   TEXT NOT NULL,
            object_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            memory_id   TEXT REFERENCES memories(id) ON DELETE SET NULL,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS wiki_pages (
            slug        TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            content     TEXT NOT NULL,
            topic       TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
            content, category, tags,
            content='memories',
            content_rowid='rowid'
        );

        CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
            INSERT INTO memories_fts(rowid, content, category, tags)
            VALUES (new.rowid, new.content, new.category, new.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
            VALUES ('delete', old.rowid, old.content, old.category, old.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
            VALUES ('delete', old.rowid, old.content, old.category, old.tags);
            INSERT INTO memories_fts(rowid, content, category, tags)
            VALUES (new.rowid, new.content, new.category, new.tags);
        END;

        CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
        CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source);
        CREATE INDEX IF NOT EXISTS idx_memories_created ON memories(created_at);
        CREATE INDEX IF NOT EXISTS idx_memories_updated ON memories(updated_at);
        ",
    )?;

    conn.execute(&format!("PRAGMA user_version = {}", SCHEMA_VERSION), [])?;
    Ok(())
}
