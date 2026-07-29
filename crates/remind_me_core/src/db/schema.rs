use crate::db::migrations;
use rusqlite::{Connection, Result};

pub use crate::db::migrations::SCHEMA_VERSION;

/// Open-time schema setup: connection pragmas, the v0 base schema, then every
/// pending migration.
///
/// The version stamp is written by [`migrations::migrate`] as each step
/// completes, never up front. Stamping a target version before the schema
/// matches it is what previously made databases written here unreadable to
/// `remind_me` — it trusts the number and skips migrating.
pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA busy_timeout=5000;
        PRAGMA foreign_keys=ON;
        ",
    )?;

    migrations::create_base_schema(conn)?;
    migrations::migrate(conn)
}
