use crate::db::migrations;
use rusqlite::{Connection, Result};

pub use crate::db::migrations::SCHEMA_VERSION;

/// Open-time setup: connection pragmas, then schema creation and reconciliation.
///
/// See [`migrations`] for how the schema is defined — it is generated from
/// `remind_me` rather than written here.
pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA busy_timeout=5000;
        PRAGMA foreign_keys=ON;
        ",
    )?;

    migrations::apply(conn)
}
