//! Coverage for `remind_me_server_status`.

use remind_me_core::backup::create_backup;
use remind_me_core::db::queries;
use remind_me_core::db::schema::SCHEMA_VERSION;
use remind_me_core::status::{server_status, SubsystemStatus};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
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
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rrm_status_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn is_missing(status: &SubsystemStatus) -> bool {
    matches!(status, SubsystemStatus::NotImplemented { .. })
}

#[test]
fn a_fresh_store_reports_a_current_schema() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(report.expected_schema_version, SCHEMA_VERSION);
    assert!(report.schema_current);
    assert_eq!(report.memory_count, 0);
}

#[test]
fn a_stale_schema_version_is_reported_as_not_current() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute_batch("PRAGMA user_version = 3;").unwrap();

    let report = server_status(&conn).unwrap();

    // A version mismatch is what makes a database unreadable to `remind_me`,
    // so it is surfaced rather than left for a caller to infer.
    assert_eq!(report.schema_version, 3);
    assert!(!report.schema_current);
}

#[test]
fn deleted_memories_are_not_counted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "kept");
    add(&conn, "going");
    let doomed: String = conn
        .query_row("SELECT id FROM memories WHERE content = 'going'", [], |r| {
            r.get(0)
        })
        .unwrap();
    queries::delete_memory(&conn, &doomed).unwrap();

    assert_eq!(server_status(&conn).unwrap().memory_count, 1);
}

#[test]
fn an_in_memory_database_has_no_path_or_backup_directory() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert!(report.database_path.is_none());
    assert!(report.database_exists, "it exists, it just has no file");
    assert!(report.database_bytes.is_none());
    // Absent rather than empty: there is no directory to hold backups.
    assert!(report.backup_dir.is_none());
    assert_eq!(report.backup_count, 0);
}

#[test]
fn an_on_disk_database_reports_its_path_and_size() {
    let dir = scratch("ondisk");
    let path = dir.join("memories.db");
    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    add(&conn, "a memory");

    let report = server_status(&conn).unwrap();

    assert_eq!(
        report.database_path.as_deref(),
        Some(path.display().to_string().as_str())
    );
    assert!(report.database_exists);
    assert!(report.database_bytes.unwrap() > 0);
    assert!(report.backup_dir.is_some());

    drop(conn);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backups_are_inventoried_newest_first() {
    let dir = scratch("backups");
    let path = dir.join("memories.db");
    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    add(&conn, "a memory");

    assert_eq!(server_status(&conn).unwrap().backup_count, 0);
    create_backup(&conn, "first").unwrap();
    create_backup(&conn, "second").unwrap();

    let report = server_status(&conn).unwrap();
    assert_eq!(report.backup_count, 2);
    let latest = report.latest_backup.unwrap();
    assert!(
        latest.filename.contains("second"),
        "newest first, got {}",
        latest.filename
    );

    drop(conn);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn absent_subsystems_are_named_with_a_reason() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert!(matches!(report.mcp, SubsystemStatus::Active));
    // Reported as not-implemented with a reason, not as "stopped". A bare
    // false could not tell "this crate has no dashboard" apart from "the
    // dashboard is down".
    for status in [&report.dashboard, &report.embeddings, &report.sync] {
        assert!(is_missing(status));
        if let SubsystemStatus::NotImplemented { reason } = status {
            assert!(!reason.is_empty(), "a reason must say something");
        }
    }
}

#[test]
fn the_report_serialises_with_the_subsystem_state_tagged() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();
    let json = serde_json::to_value(&report).unwrap();

    assert_eq!(json["mcp"]["state"], "active");
    assert_eq!(json["embeddings"]["state"], "not_implemented");
    assert!(json["embeddings"]["reason"].is_string());
    // The watcher exists now, so it reports its own state rather than being
    // listed as an absent subsystem.
    assert_eq!(json["watcher"]["enabled"], false);
    assert!(json["watcher"]["hint"].is_string());
    assert_eq!(json["schema_current"], true);
}
