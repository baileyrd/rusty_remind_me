//! SQLite connection setup.
//!
//! Mirrors the connection-configuration half of `src/dbs/storage/sqlite.py`
//! in baileyrd/Daily-Backup-System (pinned `@6cc6491`) — `SqliteStorage.
//! __init__`/`_configure`. The `Storage` trait implementation itself
//! (upsert/query methods) is out of scope for this issue; that's the
//! engine issues (#16/#17/#19/#20) building on top of this connection and
//! [`crate::storage::migrations`].

use std::path::Path;

use rusqlite::Connection;

use crate::errors::DbsError;
use crate::storage::migrations;

/// Opens a SQLite connection at `path` (or a private in-memory database
/// for `":memory:"`/`""`/any `file::memory:` prefix — this simplified
/// port doesn't honor URI query params like `?cache=shared`, unlike the
/// reference's plain `sqlite3.connect(path)`), applies the reference's
/// pragmas, and runs pending migrations.
///
/// Pragmas, matching `SqliteStorage._configure` exactly: `journal_mode
/// = WAL`, `synchronous = NORMAL`, `foreign_keys = ON`, `busy_timeout =
/// 30000` (generous — under `--parallel N`, several worker connections
/// share the single WAL writer slot, and one worker's flush must not time
/// out another's commit).
pub fn open_connection(path: &str) -> Result<Connection, DbsError> {
    let is_memory = path.is_empty() || path == ":memory:" || path.starts_with("file::memory:");

    let mut conn = if is_memory {
        Connection::open_in_memory()
            .map_err(|e| DbsError::Storage(format!("failed to open in-memory database: {e}")))?
    } else {
        let expanded = shellexpand_home(path);
        if let Some(parent) = Path::new(&expanded).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DbsError::Storage(format!(
                    "failed to create parent directory for {expanded:?}: {e}"
                ))
            })?;
        }
        Connection::open(&expanded)
            .map_err(|e| DbsError::Storage(format!("failed to open database {expanded:?}: {e}")))?
    };

    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| DbsError::Storage(format!("failed to set journal_mode: {e}")))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| DbsError::Storage(format!("failed to set synchronous: {e}")))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| DbsError::Storage(format!("failed to set foreign_keys: {e}")))?;
    conn.pragma_update(None, "busy_timeout", 30_000i64)
        .map_err(|e| DbsError::Storage(format!("failed to set busy_timeout: {e}")))?;

    migrations::migrate(&mut conn)?;
    Ok(conn)
}

/// Minimal `~` expansion (home directory only, at the start of the path)
/// — avoids pulling in a dedicated crate for a single-purpose need this
/// small; not a general shell-expansion implementation.
fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_connection_memory_variants_all_work() {
        for path in [":memory:", "", "file::memory:?cache=shared"] {
            let conn = open_connection(path).unwrap();
            let version: i64 = conn
                .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(version, migrations::SCHEMA_VERSION);
        }
    }

    #[test]
    fn open_connection_sets_expected_pragmas() {
        let conn = open_connection(":memory:").unwrap();
        let foreign_keys: i64 = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let busy_timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 30_000);
    }

    #[test]
    fn open_connection_creates_parent_directories_for_a_file_path() {
        let dir = std::env::temp_dir().join(format!("rusty_dbs_test_{}", std::process::id()));
        let db_path = dir.join("nested").join("dir").join("test.sqlite3");
        let conn = open_connection(db_path.to_str().unwrap()).unwrap();
        drop(conn);
        assert!(db_path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shellexpand_home_expands_leading_tilde() {
        if let Some(home) = std::env::var_os("HOME") {
            let expanded = shellexpand_home("~/dbs.sqlite3");
            assert_eq!(
                expanded,
                Path::new(&home).join("dbs.sqlite3").to_string_lossy()
            );
        }
    }

    #[test]
    fn shellexpand_home_leaves_absolute_paths_unchanged() {
        assert_eq!(shellexpand_home("/tmp/dbs.sqlite3"), "/tmp/dbs.sqlite3");
    }
}
