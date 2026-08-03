//! Coverage for outbox growth and its pruning.
//!
//! The outbox triggers arrived with the generated schema and fire on every
//! write while sync is configured (`#76`'s `sync_flags` gate — every test
//! here runs with the three sync env vars set, via [`ensure_sync_enabled`],
//! since this file is entirely about what accumulates once they do). Nothing
//! in this crate drains the outbox on its own, so without a retention rule
//! the table grows without bound — carrying a full JSON copy of the memory
//! each time.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::sync::{
    prune_outbox, DEFAULT_OUTBOX_RETENTION_DAYS, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV,
};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput, MemoryUpdateInput};
use rusqlite::Connection;

/// Every test in this file wants sync on and never off, so setting these
/// process-wide env vars needs no `ENV_LOCK`-style guard against other tests
/// in the same binary racing it to a different value — there is no
/// different value any test here ever wants.
fn ensure_sync_enabled() {
    std::env::set_var(NODE_ID_ENV, "node-outbox-test");
    std::env::set_var(HUB_URL_ENV, "http://hub.example");
    std::env::set_var(SYNC_SECRET_ENV, "shh");
}

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
    ensure_sync_enabled();
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
fn reads_do_not_reach_the_outbox() {
    ensure_sync_enabled();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting");
    let after_write = outbox_rows(&conn);

    search(&conn, "quokka");

    // Recording access is an UPDATE, so the update trigger sees it -- but the
    // trigger's `NEW.updated_at IS NOT OLD.updated_at` guard rejects it,
    // because access tracking deliberately leaves `updated_at` alone. Without
    // that guard every read enqueued a payload identical to the one already
    // sitting in the outbox, and outbox depth tracked read volume rather than
    // pending work.
    assert_eq!(
        outbox_rows(&conn),
        after_write,
        "recording access is not a change worth syncing"
    );
}

#[test]
fn edits_still_reach_the_outbox() {
    ensure_sync_enabled();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting");
    let after_write = outbox_rows(&conn);

    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            memory_id: id,
            content: Some("quokka sighting, confirmed".into()),
            category: None,
            tags: None,
            metadata: None,
        },
    )
    .unwrap();

    // The guard keys on `updated_at`, so it has to let a genuine edit through:
    // suppressing reads is only correct if it costs no real change.
    assert_eq!(
        outbox_rows(&conn),
        after_write + 1,
        "an edit is exactly what the outbox exists to carry"
    );
}

/// A database created before the guard was added keeps the old trigger unless
/// something drops it: the generated schema is applied with
/// `CREATE TRIGGER IF NOT EXISTS`, which silently skips a trigger that already
/// exists no matter how stale its definition.
#[test]
fn an_existing_database_has_its_stale_trigger_rebuilt() {
    ensure_sync_enabled();
    let dir = std::env::temp_dir().join(format!("rrm_outbox_rebuild_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memories.db");
    let _ = std::fs::remove_file(&path);

    {
        let db = Database::open(&path).unwrap();
        let conn = db.conn();
        // Put the pre-fix trigger back: same name, same body, no guard.
        let unguarded: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='memories_outbox_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let unguarded = unguarded.replace("AND NEW.updated_at IS NOT OLD.updated_at", "");
        conn.execute_batch(&format!("DROP TRIGGER memories_outbox_au; {};", unguarded))
            .unwrap();
    }

    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='memories_outbox_au'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql.contains("NEW.updated_at IS NOT OLD.updated_at"),
        "reopening should have replaced the stale trigger, got: {sql}"
    );

    add(&conn, "quokka sighting");
    let after_write = outbox_rows(&conn);
    search(&conn, "quokka");
    assert_eq!(
        outbox_rows(&conn),
        after_write,
        "the rebuilt trigger must actually suppress reads"
    );

    drop(conn);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn already_sent_rows_are_pruned_immediately() {
    ensure_sync_enabled();
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
    ensure_sync_enabled();
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
    ensure_sync_enabled();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "stale memory");
    backdate_outbox(&conn, DEFAULT_OUTBOX_RETENTION_DAYS + 1);

    assert_eq!(prune_outbox(&conn).unwrap(), 1);
    assert_eq!(outbox_rows(&conn), 0);
}

#[test]
fn pruning_drops_orphaned_send_markers() {
    ensure_sync_enabled();
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
    ensure_sync_enabled();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "memory one");

    assert_eq!(prune_outbox(&conn).unwrap(), 0);
    assert_eq!(prune_outbox(&conn).unwrap(), 0);
    assert_eq!(outbox_rows(&conn), 1);
}

#[test]
fn opening_a_database_prunes_it() {
    ensure_sync_enabled();
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
    ensure_sync_enabled();
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
        // Ten writes, ten rows. The searches contribute nothing: this count
        // used to exceed 200, because each search touched all ten memories'
        // access counters and every one of those updates was enqueued.
        assert_eq!(
            outbox_rows(&conn),
            10,
            "only the writes belong in the outbox"
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
