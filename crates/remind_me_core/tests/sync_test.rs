//! Coverage for the memories-only sync slice (#57): conflict resolution,
//! echo suppression, the outbox push/pull client against a real peer
//! server, and the `node_id`/`client`-stamping and soft-delete wiring that
//! turning sync on changes elsewhere in this crate.
//!
//! `REMIND_ME_NODE_ID`/`REMIND_ME_CLIENT`/`REMIND_ME_HUB_URL`/
//! `REMIND_ME_SYNC_SECRET` are process-global, and tests run concurrently by
//! default, so every test that touches any of them holds `ENV_LOCK` for the
//! duration -- the same convention `mempalace_import_test.rs` established.

use remind_me_core::db::queries;
use remind_me_core::sync::{
    self, pull_remote, push_outbox, serve_once, upsert_record, ApplyOutcome, PeerServerConfig,
    SyncRecord, CLIENT_ENV, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use serde_json::json;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

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

fn memory_row(conn: &Connection, id: &str) -> (String, Vec<String>, serde_json::Value, String) {
    conn.query_row(
        "SELECT content, tags, metadata, updated_at FROM memories WHERE id = ?",
        [id],
        |row| {
            let tags: String = row.get(1)?;
            let metadata: String = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                serde_json::from_str(&tags).unwrap(),
                serde_json::from_str(&metadata).unwrap(),
                row.get::<_, String>(3)?,
            ))
        },
    )
    .unwrap()
}

fn record(id: &str, content: &str, updated_at: &str) -> SyncRecord {
    serde_json::from_value(json!({
        "id": id,
        "content": content,
        "created_at": updated_at,
        "updated_at": updated_at,
    }))
    .unwrap()
}

// ---------------------------------------------------------------------------
// Conflict resolution
// ---------------------------------------------------------------------------

#[test]
fn a_strictly_newer_incoming_record_wins_and_replaces_content() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");

    let mut incoming = record(&id, "remote content", "2030-01-01T00:00:00+00:00");
    incoming.tags = vec!["remote-tag".to_string()];

    let outcome = upsert_record(&conn, &incoming).unwrap();

    assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
    let (content, tags, _, _) = memory_row(&conn, &id);
    assert_eq!(content, "remote content");
    assert_eq!(tags, vec!["remote-tag"]);
}

#[test]
fn tags_union_merge_regardless_of_which_side_wins() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    conn.execute(
        "UPDATE memories SET tags = '[\"local-tag\"]' WHERE id = ?",
        [&id],
    )
    .unwrap();

    let mut incoming = record(&id, "remote content", "2030-01-01T00:00:00+00:00");
    incoming.tags = vec!["remote-tag".to_string()];
    upsert_record(&conn, &incoming).unwrap();

    let (_, tags, _, _) = memory_row(&conn, &id);
    assert_eq!(
        tags,
        vec!["local-tag", "remote-tag"],
        "local tags first, then incoming's new ones"
    );
}

#[test]
fn an_older_incoming_record_loses_but_its_tag_still_merges_in() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    conn.execute(
        "UPDATE memories SET updated_at = '2030-01-01T00:00:00+00:00' WHERE id = ?",
        [&id],
    )
    .unwrap();
    let (_, _, _, updated_before) = memory_row(&conn, &id);

    let mut incoming = record(&id, "older remote content", "2020-01-01T00:00:00+00:00");
    incoming.tags = vec!["remote-tag".to_string()];

    let outcome = upsert_record(&conn, &incoming).unwrap();

    assert_eq!(outcome, ApplyOutcome::NotApplied);
    let (content, tags, _, updated_after) = memory_row(&conn, &id);
    assert_eq!(
        content, "local content",
        "the losing side's content must not overwrite local"
    );
    assert_eq!(
        tags,
        vec!["remote-tag"],
        "still merges in even though it lost"
    );
    assert_eq!(
        updated_after, updated_before,
        "a losing record must not bump updated_at"
    );
}

#[test]
fn an_equal_timestamp_means_incoming_loses() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    let (_, _, _, updated_at) = memory_row(&conn, &id);

    let incoming = record(&id, "remote content", &updated_at);
    let outcome = upsert_record(&conn, &incoming).unwrap();

    assert_eq!(
        outcome,
        ApplyOutcome::NotApplied,
        "a tie must not let the incoming side win"
    );
    assert_eq!(memory_row(&conn, &id).0, "local content");
}

#[test]
fn metadata_shallow_merge_the_winner_takes_a_conflicting_key() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    conn.execute(
        "UPDATE memories SET metadata = '{\"shared\":\"local-value\",\"local_only\":\"kept\"}' WHERE id = ?",
        [&id],
    )
    .unwrap();

    let mut incoming = record(&id, "remote content", "2030-01-01T00:00:00+00:00");
    incoming.metadata = json!({"shared": "remote-value", "remote_only": "kept"});
    upsert_record(&conn, &incoming).unwrap();

    let (_, _, metadata, _) = memory_row(&conn, &id);
    assert_eq!(
        metadata,
        json!({"shared": "remote-value", "local_only": "kept", "remote_only": "kept"})
    );
}

#[test]
fn metadata_merge_when_incoming_loses_keeps_the_local_value_on_the_shared_key() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    conn.execute("UPDATE memories SET updated_at = '2030-01-01T00:00:00+00:00', metadata = '{\"shared\":\"local-value\"}' WHERE id = ?", [&id]).unwrap();

    let mut incoming = record(&id, "older remote content", "2020-01-01T00:00:00+00:00");
    incoming.metadata = json!({"shared": "remote-value", "remote_only": "kept"});
    upsert_record(&conn, &incoming).unwrap();

    let (_, _, metadata, _) = memory_row(&conn, &id);
    assert_eq!(
        metadata,
        json!({"shared": "local-value", "remote_only": "kept"})
    );
}

#[test]
fn a_brand_new_remote_id_is_inserted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let incoming = record(
        "mem_remote_1",
        "brand new remote content",
        "2026-01-01T00:00:00+00:00",
    );

    let outcome = upsert_record(&conn, &incoming).unwrap();

    assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
    assert_eq!(
        memory_row(&conn, "mem_remote_1").0,
        "brand new remote content"
    );
}

#[test]
fn a_record_missing_a_required_field_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut incoming = record("mem_bad", "content", "2026-01-01T00:00:00+00:00");
    incoming.content = String::new();

    assert!(upsert_record(&conn, &incoming).is_err());
}

#[test]
fn winning_an_existing_row_does_not_touch_created_at() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    let original_created_at: String = conn
        .query_row("SELECT created_at FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();

    let mut incoming = record(&id, "remote content", "2030-01-01T00:00:00+00:00");
    incoming.created_at = "1999-01-01T00:00:00+00:00".to_string();
    upsert_record(&conn, &incoming).unwrap();

    let created_at_after: String = conn
        .query_row("SELECT created_at FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        created_at_after, original_created_at,
        "created_at is insert-only, never overwritten on update"
    );
}

// ---------------------------------------------------------------------------
// Echo suppression
// ---------------------------------------------------------------------------

fn outbox_row_count(conn: &Connection, memory_id: &str, sent: bool) -> i64 {
    let clause = if sent {
        "sent_at != ''"
    } else {
        "sent_at = ''"
    };
    conn.query_row(
        &format!("SELECT count(*) FROM sync_outbox WHERE memory_id = ? AND {clause}"),
        [memory_id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn applying_an_incoming_record_marks_only_its_own_echo_as_sent() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var(NODE_ID_ENV, "local-node");
    std::env::set_var(HUB_URL_ENV, "http://hub.example");
    std::env::set_var(SYNC_SECRET_ENV, "shh");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "local content");
    // A genuinely concurrent local edit, unrelated to the incoming pull.
    conn.execute(
        "UPDATE memories SET content = 'a local edit' WHERE id = ?",
        [&id],
    )
    .unwrap();
    assert_eq!(
        outbox_row_count(&conn, &id, false),
        2,
        "add + the local edit both queued"
    );

    let incoming = record(&id, "remote content", "2030-01-01T00:00:00+00:00");
    upsert_record(&conn, &incoming).unwrap();

    // The local edit's own outbox row must survive unsuppressed -- only the
    // row this very upsert just created is echo-suppressed.
    assert_eq!(
        outbox_row_count(&conn, &id, false),
        2,
        "the two pre-existing rows are untouched"
    );
    assert_eq!(
        outbox_row_count(&conn, &id, true),
        1,
        "only this write's own echo is marked sent"
    );
    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);
}

// ---------------------------------------------------------------------------
// node_id/client stamping and soft-delete (env-gated behavior)
// ---------------------------------------------------------------------------

#[test]
fn add_memory_stamps_the_configured_node_id_and_client() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var(NODE_ID_ENV, "node-a");
    std::env::set_var(CLIENT_ENV, "laptop");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "stamped content");

    let (node_id, client): (String, String) = conn
        .query_row(
            "SELECT node_id, client FROM memories WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(CLIENT_ENV);
    assert_eq!(node_id, "node-a");
    assert_eq!(client, "laptop");
}

#[test]
fn add_memory_stamps_empty_node_id_and_unknown_client_by_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(CLIENT_ENV);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "unstamped content");

    let (node_id, client): (String, String) = conn
        .query_row(
            "SELECT node_id, client FROM memories WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(node_id, "");
    assert_eq!(client, "unknown");
}

#[test]
fn delete_is_a_hard_delete_when_sync_is_not_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);
    assert!(!sync::sync_enabled());

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "doomed content");

    assert!(queries::delete_memory(&conn, &id).unwrap());

    let remaining: i64 = conn
        .query_row("SELECT count(*) FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        remaining, 0,
        "no row at all -- a real DELETE, not a tombstone"
    );
}

#[test]
fn delete_tombstones_instead_of_hard_deleting_when_sync_is_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var(NODE_ID_ENV, "node-a");
    std::env::set_var(HUB_URL_ENV, "http://hub.example:8766");
    std::env::set_var(SYNC_SECRET_ENV, "s3cret");
    assert!(sync::sync_enabled());

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "doomed content");

    let deleted = queries::delete_memory(&conn, &id).unwrap();

    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);

    assert!(deleted);
    let (deleted_at, updated_at): (Option<String>, String) = conn
        .query_row(
            "SELECT deleted_at, updated_at FROM memories WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(deleted_at.is_some(), "the row survives, tombstoned");
    assert!(!updated_at.is_empty());
    // Excluded from the normal read path exactly like a hard delete would be.
    assert!(queries::get_memory_by_id(&conn, &id).unwrap().is_none());
}

#[test]
fn a_second_delete_of_an_already_tombstoned_memory_reports_not_found() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var(NODE_ID_ENV, "node-a");
    std::env::set_var(HUB_URL_ENV, "http://hub.example:8766");
    std::env::set_var(SYNC_SECRET_ENV, "s3cret");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "doomed content");
    queries::delete_memory(&conn, &id).unwrap();

    let second = queries::delete_memory(&conn, &id).unwrap();

    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);
    assert!(
        !second,
        "an already-tombstoned memory is not live to delete again"
    );
}

// ---------------------------------------------------------------------------
// Push/pull against a real peer server (no HTTP mocking: a second real
// in-memory Database served by the actual `serve_once` handler over a real
// TcpListener -- the same protocol this node's own worker uses against a
// configured hub).
// ---------------------------------------------------------------------------

const SECRET: &str = "hub-secret";

struct TestHub {
    url: String,
    db: Arc<Database>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TestHub {
    fn start(node_id: &str) -> Self {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = PeerServerConfig::new("127.0.0.1", port, SECRET, node_id);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_db = Arc::clone(&db);
        let handle = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let conn = thread_db.conn();
                        let _ = serve_once(&mut stream, &config, &conn);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://127.0.0.1:{}", port),
            db,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// These tests only need a row sitting in `sync_outbox` before calling
/// `push_outbox` -- `#76`'s `sync_flags` gate means a plain `add()` no
/// longer queues one unless sync is actually configured, so every test here
/// that pushes a locally-written memory holds `ENV_LOCK` and sets the three
/// sync env vars first.
fn enable_sync(node_id: &str) {
    std::env::set_var(NODE_ID_ENV, node_id);
    std::env::set_var(HUB_URL_ENV, "http://hub.example");
    std::env::set_var(SYNC_SECRET_ENV, SECRET);
}

fn disable_sync() {
    std::env::remove_var(NODE_ID_ENV);
    std::env::remove_var(HUB_URL_ENV);
    std::env::remove_var(SYNC_SECRET_ENV);
}

#[test]
fn push_outbox_delivers_local_writes_to_a_real_hub() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("local-node");
    let hub = TestHub::start("hub-node");
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();
    let id = add(&local_conn, "pushed content");

    let report = push_outbox(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();

    assert_eq!(report.pushed, 1);
    let hub_conn = hub.db.conn();
    let hub_content: String = hub_conn
        .query_row("SELECT content FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(hub_content, "pushed content");

    // A second push cycle has nothing left to send.
    let second = push_outbox(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();
    assert_eq!(
        second.pushed, 0,
        "already-acknowledged rows are not re-sent"
    );
    disable_sync();
}

#[test]
fn push_outbox_reports_a_clear_error_when_the_hub_is_unreachable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("local-node");
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();
    add(&local_conn, "content");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let err = push_outbox(
        &local_conn,
        &format!("http://127.0.0.1:{port}"),
        SECRET,
        "local-node",
        "hub",
    )
    .unwrap_err();
    assert!(!err.to_string().is_empty());
    disable_sync();
}

#[test]
fn push_outbox_is_refused_with_the_wrong_secret() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("local-node");
    let hub = TestHub::start("hub-node");
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();
    add(&local_conn, "content");

    let err = push_outbox(&local_conn, &hub.url, "wrong-secret", "local-node", "hub").unwrap_err();
    assert!(err.to_string().contains("401"), "got {err}");
    disable_sync();
}

#[test]
fn pull_remote_applies_the_hubs_changes_and_persists_the_cursor() {
    let hub = TestHub::start("hub-node");
    {
        let hub_conn = hub.db.conn();
        add(&hub_conn, "hub content");
    }
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();

    let report = pull_remote(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();

    assert_eq!(report.applied, 1);
    let count: i64 = local_conn
        .query_row(
            "SELECT count(*) FROM memories WHERE content = 'hub content'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    let (last_pull, last_pull_id): (String, String) = local_conn
        .query_row(
            "SELECT last_pull, last_pull_id FROM sync_log WHERE remote_id = 'hub'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_ne!(last_pull, "1970-01-01T00:00:00+00:00");
    assert!(!last_pull_id.is_empty());

    // A second pull with nothing new does not re-apply anything.
    let second = pull_remote(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();
    assert_eq!(second.applied, 0);
}

#[test]
fn pull_remote_excludes_records_this_node_originated() {
    let hub = TestHub::start("hub-node");
    {
        let hub_conn = hub.db.conn();
        let id = add(&hub_conn, "originated locally then pushed to hub");
        hub_conn
            .execute(
                "UPDATE memories SET node_id = 'local-node' WHERE id = ?",
                [&id],
            )
            .unwrap();
    }
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();

    let report = pull_remote(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();

    assert_eq!(
        report.applied, 0,
        "a record this node originated must not be pulled back to itself"
    );
}

#[test]
fn a_full_push_then_pull_round_trip_between_two_nodes_converges() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let hub = TestHub::start("hub-node");
    let node_a = Database::open_in_memory().unwrap();
    let node_a_conn = node_a.conn();
    add(&node_a_conn, "from node a");

    push_outbox(&node_a_conn, &hub.url, SECRET, "node-a", "hub").unwrap();

    let node_b = Database::open_in_memory().unwrap();
    let node_b_conn = node_b.conn();
    let report = pull_remote(&node_b_conn, &hub.url, SECRET, "node-b", "hub").unwrap();

    assert_eq!(report.applied, 1);
    let count: i64 = node_b_conn
        .query_row(
            "SELECT count(*) FROM memories WHERE content = 'from node a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    disable_sync();
}
