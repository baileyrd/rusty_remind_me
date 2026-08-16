//! Ordered schema migrations for the SQLite backend.
//!
//! Mirrors `src/dbs/storage/migrations.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Each migration is `(version, sql)` applied in
//! order; the migration body and the `schema_migrations` bookkeeping row
//! commit together in one transaction, via `BEGIN IMMEDIATE` (not SQLite's
//! default deferred mode) so two callers racing to open the same
//! not-yet-migrated database serialize on the write lock instead of both
//! attempting the same `CREATE TABLE`.
//!
//! Connection pragmas (WAL, `foreign_keys`, `busy_timeout`) are **not**
//! set here — same as the reference, WAL mode set inside a transaction is
//! a silent no-op — they're set per-connection by
//! [`crate::storage::sqlite::open_connection`].

use rusqlite::{Connection, TransactionBehavior};

use crate::errors::DbsError;

const MIGRATION_0001: &str = r#"
CREATE TABLE schema_migrations (
    version     INTEGER PRIMARY KEY,
    applied_at  TEXT NOT NULL
);

CREATE TABLE sources (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    type            TEXT NOT NULL,
    plugin_id       TEXT NOT NULL,
    config_json     TEXT NOT NULL DEFAULT '{}',
    schema_version  INTEGER NOT NULL DEFAULT 1,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL
);

CREATE TABLE sync_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id       INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    plugin_id       TEXT NOT NULL,
    status          TEXT NOT NULL,
    mode            TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    finished_at     TEXT,
    items_seen      INTEGER NOT NULL DEFAULT 0,
    items_created   INTEGER NOT NULL DEFAULT 0,
    items_updated   INTEGER NOT NULL DEFAULT 0,
    items_unchanged INTEGER NOT NULL DEFAULT 0,
    items_deleted   INTEGER NOT NULL DEFAULT 0,
    items_undeleted INTEGER NOT NULL DEFAULT 0,
    revisions       INTEGER NOT NULL DEFAULT 0,
    cursor_before   TEXT,
    cursor_after    TEXT,
    error           TEXT
);
CREATE INDEX idx_runs_source_started ON sync_runs(source_id, started_at DESC);
CREATE INDEX idx_runs_status         ON sync_runs(status);

CREATE TABLE items (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id       INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    external_id     TEXT NOT NULL,
    item_kind       TEXT NOT NULL,
    title           TEXT,
    url             TEXT,
    body            TEXT,
    tags_json       TEXT NOT NULL DEFAULT '[]',
    item_created_at TEXT,
    item_updated_at TEXT,
    content_hash    TEXT NOT NULL,
    raw_json        TEXT NOT NULL,
    revision        INTEGER NOT NULL DEFAULT 1,
    first_seen_at   TEXT NOT NULL,
    last_seen_at    TEXT NOT NULL,
    last_changed_at TEXT NOT NULL,
    observed_run_id INTEGER NOT NULL REFERENCES sync_runs(id) ON DELETE CASCADE,
    deleted         INTEGER NOT NULL DEFAULT 0,
    deleted_at      TEXT,
    UNIQUE(source_id, external_id)
);
CREATE INDEX idx_items_source_kind     ON items(source_id, item_kind);
CREATE INDEX idx_items_source_deleted  ON items(source_id, deleted);
CREATE INDEX idx_items_source_observed ON items(source_id, observed_run_id);
CREATE INDEX idx_items_source_created  ON items(source_id, item_created_at);
CREATE INDEX idx_items_content_hash    ON items(source_id, content_hash);

CREATE TABLE item_revisions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id         INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    revision        INTEGER NOT NULL,
    content_hash    TEXT NOT NULL,
    raw_json        TEXT NOT NULL,
    title           TEXT,
    captured_at     TEXT NOT NULL,
    captured_run_id INTEGER NOT NULL REFERENCES sync_runs(id) ON DELETE CASCADE,
    change_kind     TEXT NOT NULL,
    UNIQUE(item_id, revision)
);
CREATE INDEX idx_revisions_item ON item_revisions(item_id, revision);

CREATE TABLE media (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id         INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    url             TEXT NOT NULL,
    kind            TEXT NOT NULL DEFAULT 'image',
    filename        TEXT,
    mime            TEXT,
    local_path      TEXT,
    sha256          TEXT,
    fetched_at      TEXT,
    UNIQUE(item_id, url)
);

CREATE TABLE sync_state (
    source_id       INTEGER PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
    cursor_json     TEXT,
    watermark       TEXT,
    run_count       INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL,
    updated_run_id  INTEGER REFERENCES sync_runs(id) ON DELETE SET NULL
);

CREATE TABLE source_locks (
    source_id       INTEGER PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
    run_id          INTEGER,
    acquired_at     TEXT NOT NULL
);
"#;

// Optionally archive the actual media bytes inline (opt-in per source via
// store_media). The reference columns (local_path/sha256/fetched_at)
// already exist from v1; this adds the blob payload + its size.
const MIGRATION_0002: &str = r#"
ALTER TABLE media ADD COLUMN data BLOB;
ALTER TABLE media ADD COLUMN byte_size INTEGER;
"#;

// "Succeeded with caveats" - a JSON array of warning strings on each run,
// kept separate from `error` so a SUCCESS run's caveats stay visible.
const MIGRATION_0003: &str = r#"
ALTER TABLE sync_runs ADD COLUMN warnings TEXT;
"#;

// Scale indexes: cross-source orderings and media blob scans had no
// covering index.
const MIGRATION_0004: &str = r#"
CREATE INDEX IF NOT EXISTS idx_items_created_global
    ON items(item_created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_media_with_data
    ON media(item_id) WHERE data IS NOT NULL;
"#;

// Per-run observability: run duration and a connector-reported failure
// count, previously visible only in logs.
const MIGRATION_0005: &str = r#"
ALTER TABLE sync_runs ADD COLUMN duration_ms INTEGER;
ALTER TABLE sync_runs ADD COLUMN items_failed INTEGER NOT NULL DEFAULT 0;
"#;

// item_updated_at filtering needs the same source-prefixed index
// item_created_at already had from v1.
const MIGRATION_0006: &str = r#"
CREATE INDEX IF NOT EXISTS idx_items_source_updated ON items(source_id, item_updated_at);
"#;

/// `(version, sql)` in ascending order.
pub const MIGRATIONS: &[(i64, &str)] = &[
    (1, MIGRATION_0001),
    (2, MIGRATION_0002),
    (3, MIGRATION_0003),
    (4, MIGRATION_0004),
    (5, MIGRATION_0005),
    (6, MIGRATION_0006),
];

pub const SCHEMA_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].0;

fn applied_versions(conn: &Connection) -> rusqlite::Result<std::collections::HashSet<i64>> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(std::collections::HashSet::new());
    }
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    rows.collect()
}

fn split_statements(sql: &str) -> Vec<&str> {
    sql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Applies pending migrations in order. Returns the versions applied.
///
/// Each pending migration re-checks `applied_versions` *after* acquiring
/// the write lock (`BEGIN IMMEDIATE`), not just once up front — mirrors
/// the reference's race-safety note: two callers opening the same
/// not-yet-migrated database concurrently must serialize on the write
/// lock rather than both attempting the same `CREATE TABLE`.
pub fn migrate(conn: &mut Connection) -> Result<Vec<i64>, DbsError> {
    let mut newly = Vec::new();
    for &(version, sql) in MIGRATIONS {
        let now = crate::timeutil::iso_z(chrono::Utc::now());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| {
                DbsError::Storage(format!("failed to begin migration transaction: {e}"))
            })?;

        let already_applied = applied_versions(&tx)
            .map_err(|e| DbsError::Storage(format!("failed to read applied migrations: {e}")))?
            .contains(&version);
        if already_applied {
            // No explicit rollback call needed — dropping `tx` without
            // `commit()` rolls back automatically.
            continue;
        }

        for statement in split_statements(sql) {
            tx.execute(statement, []).map_err(|e| {
                DbsError::Storage(format!(
                    "migration {version} failed on statement {statement:?}: {e}"
                ))
            })?;
        }
        tx.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, now],
        )
        .map_err(|e| DbsError::Storage(format!("failed to record migration {version}: {e}")))?;
        tx.commit()
            .map_err(|e| DbsError::Storage(format!("failed to commit migration {version}: {e}")))?;
        newly.push(version);
    }
    Ok(newly)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_applies_all_migrations_to_a_fresh_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = migrate(&mut conn).unwrap();
        assert_eq!(applied, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let second_pass = migrate(&mut conn).unwrap();
        assert!(second_pass.is_empty());
    }

    #[test]
    fn migrate_creates_all_expected_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in [
            "schema_migrations",
            "sources",
            "sync_runs",
            "items",
            "item_revisions",
            "media",
            "sync_state",
            "source_locks",
        ] {
            assert!(
                tables.contains(&expected.to_string()),
                "missing table {expected}"
            );
        }
    }

    #[test]
    fn migration_0002_columns_exist() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        // ALTER TABLE columns exist iff this insert (with those columns)
        // succeeds against the real schema.
        conn.execute(
            "INSERT INTO sources(name, type, plugin_id, created_at) VALUES ('t','t','t','2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_runs(source_id, plugin_id, status, mode, started_at) VALUES (1,'t','success','incremental','2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items(source_id, external_id, item_kind, content_hash, raw_json, first_seen_at, last_seen_at, last_changed_at, observed_run_id) VALUES (1,'e1','post','h','{}','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media(item_id, url, data, byte_size) VALUES (1, 'http://x', X'00', 1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn schema_version_matches_the_last_migration() {
        assert_eq!(SCHEMA_VERSION, 6);
    }

    #[test]
    fn split_statements_ignores_blank_entries() {
        assert_eq!(
            split_statements("CREATE TABLE a(x);  ; CREATE TABLE b(y);"),
            vec!["CREATE TABLE a(x)", "CREATE TABLE b(y)"]
        );
    }
}
