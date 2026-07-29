//! Coverage for outbox growth and its pruning.
//!
//! The outbox triggers arrived with the generated schema and fire on every
//! write. Nothing in this crate drains them, so without a retention rule the
//! table grows without bound — carrying a full JSON copy of the memory each
//! time.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::sync::{prune_outbox, DEFAULT_OUTBOX_RETENTION_DAYS};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str) -> String {
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
    .unwrap()
    .id
}

fn search(conn: &Connection, query: &str) {
    queries::search_with_expansions(
        conn,
        &MemorySearchInput {
            query: query.to_string(),
            category: None,
            tags: None,
            limit: 20,
            token_budget: 100_000,
            response_format: Default::default(),
            include_dormant: true,
            min_vitality: 0.0,
            verbose: false,
            expand_entities: false,
            include_neighbors: false,
            expand_co_retrieval: false,
        },
    )
    .unwrap();
}

fn outbox_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
        .unwrap()
}

fn backdate_outbox(conn: &Connection, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    conn.execute(
        "UPDATE sync_outbox SET created_at = ?",
        rusqlite::params![when],
    )
    .unwrap();
}

#[test]
fn writes_still_reach_the_outbox() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        add(&conn, &format!("memory {}", i));
    }

    // Pruning must not amount to disabling the outbox: an unsent, in-window row
    // is exactly what a sync engine would push.
    assert_eq!(outbox_rows(&conn), 5);
}

#[test]
fn reads_grow_the_outbox_too() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting");
    let after_write = outbox_rows(&conn);

    search(&conn, "quokka");

    // Recording access is an UPDATE, and the update trigger fires on it. This
    // is why the growth is not bounded by the number of memories.
    assert!(
        outbox_rows(&conn) > after_write,
        "a search should have produced an outbox row"
    );
}

#[test]
fn already_sent_rows_are_pruned_immediately() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "memory one");
    add(&conn, "memory two");
    conn.execute(
        "UPDATE sync_outbox SET sent_at = ? WHERE id = (SELECT MIN(id) FROM sync_outbox)",
        rusqlite::params![Utc::now().to_rfc3339()],
    )
    .unwrap();

    let removed = prune_outbox(&conn).unwrap();

    // A sent row is echo-suppressed and never pushed again, so it needs no
    // retention window.
    assert_eq!(removed, 1);
    assert_eq!(outbox_rows(&conn), 1);
}

#[test]
fn rows_inside_the_retention_window_survive() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "recent memory");
    backdate_outbox(&conn, DEFAULT_OUTBOX_RETENTION_DAYS - 1);

    assert_eq!(prune_outbox(&conn).unwrap(), 0);
    assert_eq!(
        outbox_rows(&conn),
        1,
        "an intermittently-reachable remote must still be able to catch up"
    );
}

#[test]
fn rows_past_the_retention_window_are_pruned() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "stale memory");
    backdate_outbox(&conn, DEFAULT_OUTBOX_RETENTION_DAYS + 1);

    assert_eq!(prune_outbox(&conn).unwrap(), 1);
    assert_eq!(outbox_rows(&conn), 0);
}

#[test]
fn pruning_drops_orphaned_send_markers() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "memory one");
    let outbox_id: i64 = conn
        .query_row("SELECT id FROM sync_outbox", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO sync_sends (remote_id, outbox_id, sent_at) VALUES ('peer_1', ?, ?)",
        rusqlite::params![outbox_id, Utc::now().to_rfc3339()],
    )
    .unwrap();
    backdate_outbox(&conn, DEFAULT_OUTBOX_RETENTION_DAYS + 1);

    prune_outbox(&conn).unwrap();

    let sends: i64 = conn
        .query_row("SELECT count(*) FROM sync_sends", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        sends, 0,
        "a send marker for a pruned row has nothing to mark"
    );
}

#[test]
fn pruning_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "memory one");

    assert_eq!(prune_outbox(&conn).unwrap(), 0);
    assert_eq!(prune_outbox(&conn).unwrap(), 0);
    assert_eq!(outbox_rows(&conn), 1);
}

#[test]
fn opening_a_database_prunes_it() {
    let dir = std::env::temp_dir().join(format!("rrm_outbox_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memories.db");
    let _ = std::fs::remove_file(&path);

    {
        let db = Database::open(&path).unwrap();
        let conn = db.conn();
        add(&conn, "old memory");
        backdate_outbox(&conn, DEFAULT_OUTBOX_RETENTION_DAYS + 1);
        assert_eq!(outbox_rows(&conn), 1);
    }

    // Open is the only cycle this crate has, so it is where the rule runs.
    let db = Database::open(&path).unwrap();
    assert_eq!(outbox_rows(&db.conn()), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_realistic_mix_of_traffic_stays_bounded() {
    let dir = std::env::temp_dir().join(format!("rrm_outbox_mix_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memories.db");
    let _ = std::fs::remove_file(&path);

    {
        let db = Database::open(&path).unwrap();
        let conn = db.conn();
        for i in 0..10 {
            add(&conn, &format!("quokka memory {}", i));
        }
        for _ in 0..20 {
            search(&conn, "quokka");
        }
        // Everything so far is older than the window by the time we reopen.
        backdate_outbox(&conn, DEFAULT_OUTBOX_RETENTION_DAYS + 1);
        assert!(
            outbox_rows(&conn) > 100,
            "30 writes and 20 searches should have produced well over 100 rows"
        );
    }

    let db = Database::open(&path).unwrap();
    assert_eq!(
        outbox_rows(&db.conn()),
        0,
        "read and write traffic past the window must not accumulate"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
