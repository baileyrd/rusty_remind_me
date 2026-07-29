//! Schema creation and reconciliation.
//!
//! # `remind_me`'s current schema is the starting point
//!
//! The three `schema_*.sql` files beside this module are **generated verbatim**
//! from a `remind_me` database — dumped straight out of its `sqlite_master`.
//! They are not hand-written and should not be hand-edited; regenerate them
//! instead.
//!
//! An earlier version of this module transcribed the reference's 19 historical
//! migrations by hand, reconstructing each step. Three of those steps were
//! written from *this* crate's pre-existing tables rather than from the
//! reference, and the divergence went unnoticed because the parity check only
//! compared table names and `memories` columns — the verification was shaped
//! like the mistake. Generating the schema removes the transcription step that
//! produced that class of error.
//!
//! # Reconciliation, not a ladder
//!
//! This crate does not replay the reference's version history. It creates the
//! current schema and reconciles anything that differs, then stamps the version
//! the schema actually corresponds to. Concretely, on open:
//!
//! 1. tables are created if absent;
//! 2. tables whose *shape* is wrong — left over from earlier versions of this
//!    crate — are rebuilt, preserving their rows;
//! 3. any column still missing from any table is added, diffed against a
//!    pristine schema built in memory from the same SQL;
//! 4. indexes and triggers are created, after the columns they reference exist;
//! 5. derived data is backfilled, and entity ids written by earlier builds of
//!    this crate are rewritten to the reference's derivation;
//! 6. `PRAGMA user_version` is stamped.
//!
//! Every phase is idempotent, so reopening is a no-op and a partially-migrated
//! database converges.

use rusqlite::{Connection, Result};

/// The version the generated schema corresponds to.
///
/// This must track whatever `remind_me` reports for the schema the
/// `schema_*.sql` files were dumped from. It is not a number this crate is free
/// to choose: `remind_me` reads it on open and skips migrating anything already
/// at its own target, so claiming a version the schema does not match is what
/// makes a database silently unreadable to it.
pub const SCHEMA_VERSION: i32 = 19;

const SCHEMA_TABLES: &str = include_str!("schema_tables.sql");
const SCHEMA_INDEXES: &str = include_str!("schema_indexes.sql");
const SCHEMA_TRIGGERS: &str = include_str!("schema_triggers.sql");

/// Collapse a DDL string so two spellings of the same object compare equal.
fn normalise_ddl(sql: &str) -> String {
    let stripped: String = sql
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    stripped
        .replace("IF NOT EXISTS ", "")
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn table_ddl(conn: &Connection, table: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name = ?")?;
    let mut rows = stmt.query([table])?;
    Ok(match rows.next()? {
        Some(row) => Some(normalise_ddl(&row.get::<_, String>(0)?)),
        None => None,
    })
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?",
        [name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn columns_of(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

/// A pristine database holding exactly the generated schema, used to diff
/// against whatever is actually on disk.
fn pristine() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(SCHEMA_TABLES)?;
    Ok(conn)
}

/// Rebuild a table whose shape predates the generated schema.
///
/// `CREATE TABLE ... AS SELECT` is not usable here — it would not carry the
/// constraints — so the correct table is created under a temporary name, the
/// shared columns are copied across, and it is renamed into place.
fn rebuild_table(conn: &Connection, table: &str) -> Result<()> {
    // Carry across exactly the columns both shapes have. Computing the
    // intersection here rather than hard-coding it per table means a rebuild
    // cannot reference a column that does not exist on either side.
    let reference = pristine()?;
    let live = columns_of(conn, table)?;
    let want = columns_of(&reference, table)?;
    let carry: String = want
        .iter()
        .filter(|c| live.contains(c))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if carry.is_empty() {
        return Ok(());
    }
    let carry = carry.as_str();

    let create = pristine()?.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name = ?",
        [table],
        |r| r.get::<_, String>(0),
    )?;
    // Point the DDL at a scratch name. SQLite strips `IF NOT EXISTS` when it
    // stores a statement in sqlite_master, so the text read back may or may not
    // carry it and may or may not quote the name — try each shape and stop at
    // the first hit. Stopping matters: the scratch name begins with the real
    // one, so a later pattern would match inside the replacement and mangle it.
    let scratch = format!("{}__rebuild", table);
    let create_scratch = [
        format!("CREATE TABLE IF NOT EXISTS \"{}\"", table),
        format!("CREATE TABLE IF NOT EXISTS {}", table),
        format!("CREATE TABLE \"{}\"", table),
        format!("CREATE TABLE {}", table),
    ]
    .iter()
    .find(|pattern| create.contains(pattern.as_str()))
    .map(|pattern| create.replacen(pattern.as_str(), &format!("CREATE TABLE {}", scratch), 1))
    .ok_or(rusqlite::Error::QueryReturnedNoRows)?;

    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let result = (|| -> Result<()> {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", scratch))?;
        conn.execute_batch(&create_scratch)?;
        conn.execute_batch(&format!(
            "INSERT INTO {scratch} ({carry}) SELECT {carry} FROM {table};",
            scratch = scratch,
            carry = carry,
            table = table
        ))?;
        conn.execute_batch(&format!(
            "DROP TABLE {table}; ALTER TABLE {scratch} RENAME TO {table};",
            table = table,
            scratch = scratch
        ))
    })();
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    result
}

/// Whether `table`'s definition differs from the generated schema.
///
/// A pure normalised-DDL comparison. An earlier version tried to be clever —
/// treating "the live columns are a prefix of the wanted ones" as "only columns
/// are missing, so ALTER is enough" — which silently accepted a stray `UNIQUE`
/// on `entities` and cascading foreign keys on `memory_entities`, because
/// neither shows up in a column-name comparison.
///
/// Rebuilding whenever anything differs is simpler and cannot be fooled. A
/// database already matching the schema produces no difference and so is never
/// rebuilt, which is the only case where the cost would matter.
fn differs_from_schema(conn: &Connection, table: &str) -> Result<bool> {
    if !table_exists(conn, table)? {
        return Ok(false);
    }
    let reference = pristine()?;
    match (table_ddl(conn, table)?, table_ddl(&reference, table)?) {
        (Some(live), Some(want)) => Ok(live != want),
        _ => Ok(false),
    }
}

/// Add any column the generated schema has and the live database does not.
///
/// Driven by diffing against [`pristine`] rather than a hand-maintained list,
/// so a column added to the generated SQL is picked up without a matching code
/// change.
fn reconcile_columns(conn: &Connection) -> Result<()> {
    let reference = pristine()?;
    let mut stmt = reference
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND sql IS NOT NULL")?;
    let tables: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_>>()?;

    for table in tables {
        if !table_exists(conn, &table)? {
            continue;
        }
        let live = columns_of(conn, &table)?;

        let mut info = reference.prepare(&format!("PRAGMA table_info({})", table))?;
        let wanted = info.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,         // name
                r.get::<_, String>(2)?,         // declared type
                r.get::<_, i64>(3)? == 1,       // notnull
                r.get::<_, Option<String>>(4)?, // default expression
            ))
        })?;

        for row in wanted {
            let (name, decl_type, not_null, default) = row?;
            if live.contains(&name) {
                continue;
            }

            // `NOT NULL` is only carried across when the column also has a
            // default — SQLite rejects adding a NOT NULL column with no default
            // to a table that already holds rows. Every table where that case
            // arises is in LEGACY_REBUILDS and gets rebuilt instead, so relaxing
            // to nullable here cannot silently weaken a real constraint.
            let decl = match (default, not_null) {
                (Some(d), true) => format!("{} NOT NULL DEFAULT {}", decl_type, d),
                (Some(d), false) => format!("{} DEFAULT {}", decl_type, d),
                (None, _) => decl_type,
            };

            conn.execute_batch(&format!(
                "ALTER TABLE {} ADD COLUMN {} {};",
                table, name, decl
            ))?;
        }
    }
    Ok(())
}

/// Rename the column this crate used before adopting the reference's name.
///
/// A rename preserves the values; adding `accessed_at` alongside would silently
/// reset every memory's access time. The legacy name lives in a constant so a
/// bulk rename cannot collapse the guard into `accessed_at && !accessed_at` —
/// which is exactly what happened once.
fn rename_legacy_columns(conn: &Connection) -> Result<()> {
    const LEGACY_ACCESSED_AT: &str = "last_accessed_at";

    if !table_exists(conn, "memories")? {
        return Ok(());
    }
    let live = columns_of(conn, "memories")?;
    if live.iter().any(|c| c == LEGACY_ACCESSED_AT) && !live.iter().any(|c| c == "accessed_at") {
        conn.execute_batch(&format!(
            "ALTER TABLE memories RENAME COLUMN {} TO accessed_at;",
            LEGACY_ACCESSED_AT
        ))?;
    }
    Ok(())
}

/// Populate derived tables for rows that predate the triggers maintaining them.
///
/// Triggers only fire on writes that happen *after* they exist. A database
/// carrying rows from before — one written by an earlier version of this crate,
/// or by a `remind_me` at a lower schema version — would otherwise have an empty
/// `memory_tags` and empty FTS indexes, which silently returns no results rather
/// than failing.
///
/// Each backfill is guarded on the derived table being empty while its source is
/// not, so this is a no-op on every subsequent open.
fn backfill_derived(conn: &Connection, force_fts_rebuild: bool) -> Result<()> {
    let count = |table: &str| -> Result<i64> {
        conn.query_row(&format!("SELECT count(*) FROM {}", table), [], |r| r.get(0))
    };

    // Tag index. `INSERT OR IGNORE` rather than an emptiness guard, because a
    // partial backfill should still converge.
    conn.execute_batch(
        "INSERT OR IGNORE INTO memory_tags (memory_id, tag)
         SELECT m.id, je.value
           FROM memories m, json_each(m.tags) AS je
          WHERE typeof(je.value) = 'text' AND json_valid(m.tags);",
    )?;

    // External-content FTS indexes. 'rebuild' is the supported way to
    // reconstruct one, but it rescans the whole base table, so only reach for it
    // when the index is actually empty and the source is not.
    for (index, source) in [("memories_fts", "memories"), ("wiki_fts", "wiki_pages")] {
        if (force_fts_rebuild || count(index)? == 0) && count(source)? > 0 {
            conn.execute_batch(&format!(
                "INSERT INTO {index}({index}) VALUES('rebuild');",
                index = index
            ))?;
        }
    }

    Ok(())
}

/// Create and reconcile the schema, then stamp the version.
///
/// The version is written last, so a database only ever claims
/// [`SCHEMA_VERSION`] once it actually has that schema.
pub fn apply(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_TABLES)?;

    rename_legacy_columns(conn)?;

    let reference = pristine()?;
    let mut names = reference.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let tables: Vec<String> = names.query_map([], |r| r.get(0))?.collect::<Result<_>>()?;
    drop(names);

    let mut rebuilt_any = false;
    for table in &tables {
        if differs_from_schema(conn, table)? {
            rebuild_table(conn, table)?;
            rebuilt_any = true;
        }
    }

    reconcile_columns(conn)?;

    // After columns: the outbox triggers name 23 columns of `memories`, and
    // several indexes cover columns added above.
    conn.execute_batch(SCHEMA_INDEXES)?;
    conn.execute_batch(SCHEMA_TRIGGERS)?;

    backfill_derived(conn, rebuilt_any)?;

    // After the tables exist and before the version is stamped: entity ids are
    // content-derived, and this crate used to derive them differently from the
    // reference, so a database written by an earlier build needs rewriting to
    // be readable by `remind_me` at all.
    crate::entity::renormalize_entity_ids(conn)?;

    conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION))
}
