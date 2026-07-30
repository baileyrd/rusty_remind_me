use crate::db::migrations;
use crate::vitality;
use rusqlite::{Connection, Result};

pub use crate::db::migrations::SCHEMA_VERSION;

/// Open-time setup: connection pragmas, the SQL helper functions, then schema
/// creation and reconciliation.
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

    // Scalar functions live on the connection, not in the file, so every
    // connection has to register them before running a query that uses one.
    vitality::register_sql_functions(conn)?;

    migrations::apply(conn)?;

    // The graph tables (entities/entity_relations/memory_entities) have no
    // generated-schema outbox trigger of their own -- this crate's own
    // addition, installed after the generated schema, same as #49's
    // vec_embeddings table.
    crate::sync::ensure_schema(conn)?;

    // The outbox triggers came in with the generated schema and fire on every
    // write, but nothing here drains them. Applying the reference's own
    // retention rule on open keeps that from growing without bound.
    crate::sync::prune_outbox(conn)?;

    Ok(())
}
