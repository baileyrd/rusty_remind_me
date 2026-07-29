//! Ordered schema migrations, mirroring the reference's 19-step ladder.
//!
//! # Why a ladder rather than one `CREATE TABLE` block
//!
//! `PRAGMA user_version` is a promise about what the file contains. The
//! reference reads it on open and applies migrations only when
//! `current_version < SCHEMA_VERSION`, so a database claiming 19 is taken at its
//! word and skipped entirely. Previously this crate created 7 tables and then
//! stamped 19, which meant `remind_me` opening such a file would conclude it was
//! fully migrated and never create the other 14 — the stamp actively defeated
//! the interoperability it exists to serve.
//!
//! Each step below stamps its own version as it completes, so the number always
//! describes what is actually present.
//!
//! # Idempotency is load-bearing
//!
//! Every step is written to be safe to re-run: `IF NOT EXISTS` throughout, and
//! [`add_column_if_missing`] for `ALTER TABLE`. That is what makes the repair
//! path in [`migrate`] possible — databases written by earlier versions of this
//! crate carry a *false* 19 with a 7-table schema, and cannot be identified by
//! version alone. They are detected by inspecting the schema and healed by
//! replaying the ladder, which is only safe because replaying is a no-op for
//! anything already present.

use rusqlite::{Connection, Result};

/// Target schema version. Increment only alongside a new step in [`migrate`].
pub const SCHEMA_VERSION: i32 = 19;

/// Tables that must exist at [`SCHEMA_VERSION`], used to detect a false stamp.
///
/// `memories_vec` is deliberately absent: the reference creates it only when the
/// `sqlite-vec` extension loads, so it is not part of the base schema.
const EXPECTED_TABLES: [&str; 21] = [
    "memories",
    "memories_fts",
    "chat_imports",
    "memory_tags",
    "memory_feedback",
    "memory_associations",
    "entities",
    "memory_entities",
    "entity_relations",
    "wiki_pages",
    "wiki_fts",
    "wiki_links",
    "wiki_meta",
    "sync_log",
    "sync_outbox",
    "sync_sends",
    "sync_flags",
    "vec_chunks",
    "embedding_meta",
    "dbs_imports",
    "mempalace_imports",
];

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?",
        [name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    if !table_exists(conn, table)? {
        return Ok(false);
    }
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `ALTER TABLE ... ADD COLUMN`, skipped when the column is already there.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, and re-adding raises rather than
/// no-opping, so the check has to happen here for steps to stay replayable.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    if !column_exists(conn, table, column)? {
        conn.execute_batch(&format!(
            "ALTER TABLE {} ADD COLUMN {} {};",
            table, column, decl
        ))?;
    }
    Ok(())
}

/// Whether the schema actually matches what [`SCHEMA_VERSION`] claims.
///
/// Checks tables and the `memories` columns most likely to be absent from a
/// database written before the ladder existed.
fn schema_is_complete(conn: &Connection) -> Result<bool> {
    for table in EXPECTED_TABLES {
        if !table_exists(conn, table)? {
            return Ok(false);
        }
    }
    for column in [
        "base_weight",
        "memory_type",
        "status",
        "accessed_at",
        "node_id",
    ] {
        if !column_exists(conn, "memories", column)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Apply every pending migration, stamping each version as it completes.
///
/// Runs from whatever `PRAGMA user_version` reports. A database that claims
/// [`SCHEMA_VERSION`] but does not actually have the schema — the false stamp
/// this crate used to write — is detected by [`schema_is_complete`] and replayed
/// from zero. Every step is idempotent, so replaying only fills the gaps.
pub fn migrate(conn: &Connection) -> Result<()> {
    let stamped: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    let mut version = if stamped >= SCHEMA_VERSION && !schema_is_complete(conn)? {
        // The stamp is a lie. Replay rather than trust it.
        0
    } else {
        stamped
    };

    // Each arm mirrors one reference migration. Steps are grouped exactly as
    // upstream so a version number means the same thing in both codebases.
    if version < 1 {
        v0_to_v1(conn)?;
        stamp(conn, 1)?;
        version = 1;
    }
    if version < 2 {
        v1_to_v2(conn)?;
        stamp(conn, 2)?;
        version = 2;
    }
    if version < 3 {
        v2_to_v3(conn)?;
        stamp(conn, 3)?;
        version = 3;
    }
    if version < 4 {
        v3_to_v4(conn)?;
        stamp(conn, 4)?;
        version = 4;
    }
    if version < 5 {
        v4_to_v5(conn)?;
        stamp(conn, 5)?;
        version = 5;
    }
    if version < 6 {
        v5_to_v6(conn)?;
        stamp(conn, 6)?;
        version = 6;
    }
    if version < 7 {
        v6_to_v7(conn)?;
        stamp(conn, 7)?;
        version = 7;
    }
    if version < 8 {
        v7_to_v8(conn)?;
        stamp(conn, 8)?;
        version = 8;
    }
    if version < 9 {
        v8_to_v9(conn)?;
        stamp(conn, 9)?;
        version = 9;
    }
    if version < 10 {
        v9_to_v10(conn)?;
        stamp(conn, 10)?;
        version = 10;
    }
    if version < 11 {
        v10_to_v11(conn)?;
        stamp(conn, 11)?;
        version = 11;
    }
    if version < 12 {
        v11_to_v12(conn)?;
        stamp(conn, 12)?;
        version = 12;
    }
    if version < 13 {
        v12_to_v13(conn)?;
        stamp(conn, 13)?;
        version = 13;
    }
    if version < 14 {
        v13_to_v14(conn)?;
        stamp(conn, 14)?;
        version = 14;
    }
    if version < 15 {
        v14_to_v15(conn)?;
        stamp(conn, 15)?;
        version = 15;
    }
    if version < 16 {
        v15_to_v16(conn)?;
        stamp(conn, 16)?;
        version = 16;
    }
    if version < 17 {
        v16_to_v17(conn)?;
        stamp(conn, 17)?;
        version = 17;
    }
    if version < 18 {
        v17_to_v18(conn)?;
        stamp(conn, 18)?;
        version = 18;
    }
    if version < 19 {
        v18_to_v19(conn)?;
        stamp(conn, 19)?;
    }

    Ok(())
}

fn stamp(conn: &Connection, version: i32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {};", version))
}

/// v0: the original schema — memories, its FTS index, and the import ledger.
pub fn create_base_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memories (
            id          TEXT PRIMARY KEY,
            content     TEXT NOT NULL,
            category    TEXT NOT NULL DEFAULT 'general',
            tags        TEXT NOT NULL DEFAULT '[]',
            source      TEXT NOT NULL DEFAULT 'manual',
            metadata    TEXT NOT NULL DEFAULT '{}',
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS chat_imports (
            import_id   TEXT PRIMARY KEY,
            filename    TEXT NOT NULL,
            hash        TEXT NOT NULL,
            imported_at TEXT NOT NULL,
            stats       TEXT NOT NULL DEFAULT '{}'
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
        ",
    )
}

/// v1: `capture_id` for linking a memory back to the capture it came from.
fn v0_to_v1(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "memories", "capture_id", "TEXT DEFAULT NULL")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_capture_id ON memories(capture_id);",
    )
}

/// v2: normalized `memory_tags` index, kept in step with the JSON `tags` column
/// by triggers rather than by callers.
fn v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memory_tags (
            memory_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            tag        TEXT NOT NULL,
            PRIMARY KEY (memory_id, tag)
        );
        CREATE INDEX IF NOT EXISTS idx_memory_tags_tag ON memory_tags(tag);

        CREATE TRIGGER IF NOT EXISTS memories_tags_ai AFTER INSERT ON memories
        BEGIN
            INSERT OR IGNORE INTO memory_tags (memory_id, tag)
            SELECT NEW.id, je.value
              FROM json_each(NEW.tags) AS je
             WHERE typeof(je.value) = 'text'
               AND json_valid(NEW.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS memories_tags_au AFTER UPDATE OF tags ON memories
        BEGIN
            DELETE FROM memory_tags WHERE memory_id = OLD.id;
            INSERT OR IGNORE INTO memory_tags (memory_id, tag)
            SELECT NEW.id, je.value
              FROM json_each(NEW.tags) AS je
             WHERE typeof(je.value) = 'text'
               AND json_valid(NEW.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS memories_tags_ad AFTER DELETE ON memories
        BEGIN
            DELETE FROM memory_tags WHERE memory_id = OLD.id;
        END;
        ",
    )?;

    // Backfill rows that predate the triggers.
    conn.execute_batch(
        "
        INSERT OR IGNORE INTO memory_tags (memory_id, tag)
        SELECT m.id, je.value
          FROM memories m, json_each(m.tags) AS je
         WHERE typeof(je.value) = 'text' AND json_valid(m.tags);
        ",
    )
}

/// v3: sync identity and the outbox ledger.
fn v2_to_v3(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "memories", "node_id", "TEXT DEFAULT NULL")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sync_log (
            remote_id   TEXT NOT NULL,
            last_pull   TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00',
            last_push   TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00',
            PRIMARY KEY (remote_id)
        );

        CREATE TABLE IF NOT EXISTS sync_outbox (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id   TEXT NOT NULL,
            operation   TEXT NOT NULL,
            payload     TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            sent_at     TEXT DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS idx_outbox_memory_id ON sync_outbox(memory_id);
        CREATE INDEX IF NOT EXISTS idx_outbox_created_at ON sync_outbox(created_at);
        ",
    )
}

/// v4: which client wrote a memory.
fn v3_to_v4(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "memories",
        "client",
        "TEXT NOT NULL DEFAULT 'unknown'",
    )?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_memories_client ON memories(client);")
}

/// v5: the ACT-R decay and classification columns.
///
/// `accessed_at` is the reference's name. Earlier versions of this crate called
/// the same thing `accessed_at`, so an existing database is renamed rather
/// than given a second, redundant column — a rename preserves the values, where
/// adding a column would silently reset every memory's access time.
fn v4_to_v5(conn: &Connection) -> Result<()> {
    // The legacy name this crate used before adopting the reference's. Spelled
    // via a constant so a future bulk rename cannot quietly collapse this guard
    // into `accessed_at && !accessed_at`, which is always false — exactly what
    // happened once while writing this.
    const LEGACY_ACCESSED_AT: &str = "last_accessed_at";

    if column_exists(conn, "memories", LEGACY_ACCESSED_AT)?
        && !column_exists(conn, "memories", "accessed_at")?
    {
        conn.execute_batch(&format!(
            "ALTER TABLE memories RENAME COLUMN {} TO accessed_at;",
            LEGACY_ACCESSED_AT
        ))?;
    }

    add_column_if_missing(conn, "memories", "accessed_at", "TEXT DEFAULT NULL")?;
    add_column_if_missing(
        conn,
        "memories",
        "access_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "memories", "decay_rate", "REAL NOT NULL DEFAULT 0.1")?;
    add_column_if_missing(conn, "memories", "vitality", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "memories", "base_weight", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "memories", "status", "TEXT NOT NULL DEFAULT 'active'")?;
    add_column_if_missing(
        conn,
        "memories",
        "memory_type",
        "TEXT NOT NULL DEFAULT 'unclassified'",
    )?;

    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_memories_vitality ON memories(vitality);
        CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);
        CREATE INDEX IF NOT EXISTS idx_memories_memory_type ON memories(memory_type);
        ",
    )
}

/// v6: linkage from a decomposed fact back to its source capture.
fn v5_to_v6(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "memories", "source_capture_id", "TEXT DEFAULT NULL")
}

/// v7: the structured subject/predicate/object triple and supersession.
fn v6_to_v7(conn: &Connection) -> Result<()> {
    for (column, decl) in [
        ("subject", "TEXT DEFAULT NULL"),
        ("predicate", "TEXT DEFAULT NULL"),
        ("object", "TEXT DEFAULT NULL"),
        ("superseded_by", "TEXT DEFAULT NULL"),
    ] {
        add_column_if_missing(conn, "memories", column, decl)?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_memories_subject ON memories(subject);")?;
    create_outbox_triggers(conn)
}

/// Triggers that record every memory write into `sync_outbox`.
///
/// Created here rather than earlier because the payload names all 23 columns it
/// carries, and every one of them exists only once v7 has run. The reference
/// drops and recreates these at each step that adds a column, so its payload
/// grows step by step; the observable end state is identical and this avoids
/// six near-duplicate definitions.
///
/// These matter even though this crate has no sync layer. `remind_me` reads
/// `sync_outbox` to decide what to propagate, and its own migrations will not
/// re-add the triggers to a database already stamped 19 — so a database created
/// here without them would look fully migrated while silently never syncing
/// anything written locally. That is the same shape of failure this whole ladder
/// exists to correct.
///
/// Note the payload stops at v7's columns: `doc_id`, `chunk_index` and
/// `deleted_at` are deliberately absent, matching the reference exactly.
fn create_outbox_triggers(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TRIGGER IF EXISTS memories_outbox_ai;
        DROP TRIGGER IF EXISTS memories_outbox_au;

        CREATE TRIGGER memories_outbox_ai AFTER INSERT ON memories BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (
                NEW.id, 'insert',
                json_object(
                    'id',                 NEW.id,
                    'content',            NEW.content,
                    'category',           NEW.category,
                    'tags',               NEW.tags,
                    'source',             NEW.source,
                    'metadata',           NEW.metadata,
                    'created_at',         NEW.created_at,
                    'updated_at',         NEW.updated_at,
                    'capture_id',         NEW.capture_id,
                    'node_id',            NEW.node_id,
                    'client',             NEW.client,
                    'accessed_at',        NEW.accessed_at,
                    'access_count',       NEW.access_count,
                    'decay_rate',         NEW.decay_rate,
                    'vitality',           NEW.vitality,
                    'base_weight',        NEW.base_weight,
                    'status',             NEW.status,
                    'memory_type',        NEW.memory_type,
                    'source_capture_id',  NEW.source_capture_id,
                    'subject',            NEW.subject,
                    'predicate',          NEW.predicate,
                    'object',             NEW.object,
                    'superseded_by',      NEW.superseded_by
                ),
                datetime('now', 'utc')
            );
        END;

        CREATE TRIGGER memories_outbox_au AFTER UPDATE ON memories BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (
                NEW.id, 'update',
                json_object(
                    'id',                 NEW.id,
                    'content',            NEW.content,
                    'category',           NEW.category,
                    'tags',               NEW.tags,
                    'source',             NEW.source,
                    'metadata',           NEW.metadata,
                    'created_at',         NEW.created_at,
                    'updated_at',         NEW.updated_at,
                    'capture_id',         NEW.capture_id,
                    'node_id',            NEW.node_id,
                    'client',             NEW.client,
                    'accessed_at',        NEW.accessed_at,
                    'access_count',       NEW.access_count,
                    'decay_rate',         NEW.decay_rate,
                    'vitality',           NEW.vitality,
                    'base_weight',        NEW.base_weight,
                    'status',             NEW.status,
                    'memory_type',        NEW.memory_type,
                    'source_capture_id',  NEW.source_capture_id,
                    'subject',            NEW.subject,
                    'predicate',          NEW.predicate,
                    'object',             NEW.object,
                    'superseded_by',      NEW.superseded_by
                ),
                datetime('now', 'utc')
            );
        END;
        ",
    )
}

/// v8: chunk map for multi-vector embeddings.
fn v7_to_v8(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS vec_chunks (
            vec_rowid    INTEGER PRIMARY KEY,
            memory_rowid INTEGER NOT NULL,
            chunk_ix     INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_vec_chunks_memory ON vec_chunks(memory_rowid);
        ",
    )
}

/// v9: per-remote send tracking and sync flags.
fn v8_to_v9(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "sync_log", "last_pull_id", "TEXT NOT NULL DEFAULT ''")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sync_sends (
            remote_id  TEXT NOT NULL,
            outbox_id  INTEGER NOT NULL,
            sent_at    TEXT NOT NULL,
            PRIMARY KEY (remote_id, outbox_id)
        );

        CREATE TABLE IF NOT EXISTS sync_flags (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_outbox_unsent ON sync_outbox(sent_at);
        CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);
        ",
    )
}

/// v10: the entity graph.
fn v9_to_v10(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
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

        CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(name);
        CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(kind);
        CREATE INDEX IF NOT EXISTS idx_entities_updated_at ON entities(updated_at);
        CREATE INDEX IF NOT EXISTS idx_memory_entities_entity ON memory_entities(entity_id);
        ",
    )
}

/// v11: the wiki index — pages, their FTS index, the link graph, and the
/// compile watermark.
fn v10_to_v11(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS wiki_pages (
            slug        TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            content     TEXT NOT NULL,
            topic       TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS wiki_links (
            src_slug   TEXT NOT NULL,
            dst_slug   TEXT NOT NULL,
            dst_title  TEXT NOT NULL,
            PRIMARY KEY (src_slug, dst_slug)
        );

        CREATE TABLE IF NOT EXISTS wiki_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS wiki_fts USING fts5(
            title, content,
            content='wiki_pages',
            content_rowid='rowid'
        );

        CREATE TRIGGER IF NOT EXISTS wiki_pages_ai AFTER INSERT ON wiki_pages BEGIN
            INSERT INTO wiki_fts(rowid, title, content)
            VALUES (new.rowid, new.title, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS wiki_pages_ad AFTER DELETE ON wiki_pages BEGIN
            INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
            VALUES ('delete', old.rowid, old.title, old.content);
        END;

        CREATE TRIGGER IF NOT EXISTS wiki_pages_au AFTER UPDATE ON wiki_pages BEGIN
            INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
            VALUES ('delete', old.rowid, old.title, old.content);
            INSERT INTO wiki_fts(rowid, title, content)
            VALUES (new.rowid, new.title, new.content);
        END;

        CREATE INDEX IF NOT EXISTS idx_wiki_links_dst ON wiki_links(dst_slug);
        ",
    )?;

    // Index pages that predate the triggers.
    conn.execute_batch(
        "INSERT INTO wiki_fts(rowid, title, content)
         SELECT w.rowid, w.title, w.content FROM wiki_pages w
         WHERE w.rowid NOT IN (SELECT rowid FROM wiki_fts);",
    )
}

/// v12: MemPalace import dedup ledger.
fn v11_to_v12(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS mempalace_imports (
            drawer_id   TEXT PRIMARY KEY,
            memory_id   TEXT NOT NULL,
            imported_at TEXT NOT NULL
        );
        ",
    )
}

/// v13: document/chunk identity for neighbour-aware retrieval.
fn v12_to_v13(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "memories", "doc_id", "TEXT DEFAULT NULL")?;
    add_column_if_missing(conn, "memories", "chunk_index", "INTEGER DEFAULT NULL")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_doc_chunk ON memories(doc_id, chunk_index);",
    )
}

/// v14: typed entity-to-entity edges for multi-hop traversal.
fn v13_to_v14(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS entity_relations (
            id          TEXT PRIMARY KEY,
            subject_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            predicate   TEXT NOT NULL,
            object_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            memory_id   TEXT REFERENCES memories(id) ON DELETE SET NULL,
            created_at  TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_entity_relations_subject ON entity_relations(subject_id);
        CREATE INDEX IF NOT EXISTS idx_entity_relations_object ON entity_relations(object_id);
        CREATE INDEX IF NOT EXISTS idx_entity_relations_created_at ON entity_relations(created_at);
        ",
    )
}

/// v15: dbs import dedup ledger.
fn v14_to_v15(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS dbs_imports (
            dbs_source   TEXT NOT NULL,
            external_id  TEXT NOT NULL,
            memory_id    TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            imported_at  TEXT NOT NULL,
            PRIMARY KEY (dbs_source, external_id)
        );
        ",
    )
}

/// v16: the `deleted_at` tombstone column.
fn v15_to_v16(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "memories", "deleted_at", "TEXT DEFAULT NULL")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_deleted_at ON memories(deleted_at);",
    )
}

/// v17: query-contextual retrieval feedback.
fn v16_to_v17(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memory_feedback (
            id           TEXT PRIMARY KEY,
            memory_id    TEXT NOT NULL,
            query        TEXT NOT NULL,
            query_tokens TEXT NOT NULL,
            signal       TEXT NOT NULL CHECK (signal IN ('helpful', 'unhelpful')),
            magnitude    REAL NOT NULL,
            created_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_memory_feedback_memory_id ON memory_feedback(memory_id);
        ",
    )
}

/// v18: embedding-model versioning.
fn v17_to_v18(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS embedding_meta (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        ",
    )
}

/// v19: the co-retrieval association graph.
fn v18_to_v19(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memory_associations (
            memory_id_a TEXT NOT NULL,
            memory_id_b TEXT NOT NULL,
            weight      INTEGER NOT NULL DEFAULT 1,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (memory_id_a, memory_id_b)
        );
        CREATE INDEX IF NOT EXISTS idx_memory_associations_a ON memory_associations(memory_id_a);
        CREATE INDEX IF NOT EXISTS idx_memory_associations_b ON memory_associations(memory_id_b);
        ",
    )
}
