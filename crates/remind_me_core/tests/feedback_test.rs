//! Coverage for `remind_me_feedback`.

use remind_me_core::db::queries;
use remind_me_core::vitality::{
    record_feedback, tokenize_query, FeedbackSignal, BASE_WEIGHT_MAX, BASE_WEIGHT_MIN,
    FEEDBACK_MAGNITUDE,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;

fn add(conn: &Connection) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: "a memory".into(),
            // "general" gives a type prior of 1.0 and manual a source prior of
            // 1.0, so base_weight starts at exactly 1.0.
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

fn base_weight(conn: &Connection, id: &str) -> f64 {
    conn.query_row(
        "SELECT base_weight FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

fn feedback_rows(conn: &Connection, id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM memory_feedback WHERE memory_id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn helpful_without_a_query_raises_the_weight_globally() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();

    assert!(
        (base_weight(&conn, &id) - (1.0 + FEEDBACK_MAGNITUDE)).abs() < 1e-9,
        "got {}",
        base_weight(&conn, &id)
    );
}

#[test]
fn unhelpful_without_a_query_lowers_the_weight_globally() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Unhelpful, None).unwrap();

    assert!((base_weight(&conn, &id) - (1.0 - FEEDBACK_MAGNITUDE)).abs() < 1e-9);
}

#[test]
fn global_feedback_writes_no_row() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();

    assert_eq!(
        feedback_rows(&conn, &id),
        0,
        "a global judgement lives in base_weight, not the log"
    );
}

#[test]
fn contextual_feedback_logs_a_row_and_leaves_the_weight_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    let before = base_weight(&conn, &id);

    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what is my favourite editor"),
    )
    .unwrap();

    assert_eq!(feedback_rows(&conn, &id), 1);
    assert!(
        (base_weight(&conn, &id) - before).abs() < 1e-9,
        "a memory can be wrong for one question and right for another; \
         contextual feedback must not demote it everywhere"
    );
}

#[test]
fn contextual_feedback_stores_normalised_query_tokens() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Helpful,
        Some("What IS my Editor?"),
    )
    .unwrap();

    let (query, tokens): (String, String) = conn
        .query_row(
            "SELECT query, query_tokens FROM memory_feedback WHERE memory_id = ?",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        query, "What IS my Editor?",
        "the raw query is kept verbatim"
    );
    // Lowercased, sorted, de-duplicated, single characters dropped.
    assert_eq!(tokens, "editor is my what");
}

#[test]
fn repeated_contextual_feedback_appends_rather_than_replacing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..3 {
        record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("same question")).unwrap();
    }

    assert_eq!(
        feedback_rows(&conn, &id),
        3,
        "the log is append-only; identical events are separate observations"
    );
}

#[test]
fn a_blank_query_is_treated_as_global() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("   ")).unwrap();

    assert_eq!(feedback_rows(&conn, &id), 0);
    assert!(
        base_weight(&conn, &id) > 1.0,
        "should have taken the global path"
    );
}

#[test]
fn repeated_helpful_feedback_is_capped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..50 {
        record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();
    }

    assert!(
        (base_weight(&conn, &id) - BASE_WEIGHT_MAX).abs() < 1e-9,
        "unbounded growth would let one memory dominate every search, got {}",
        base_weight(&conn, &id)
    );
}

#[test]
fn repeated_unhelpful_feedback_is_floored() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..100 {
        record_feedback(&conn, &id, FeedbackSignal::Unhelpful, None).unwrap();
    }

    assert!(
        (base_weight(&conn, &id) - BASE_WEIGHT_MIN).abs() < 1e-9,
        "got {}",
        base_weight(&conn, &id)
    );
}

#[test]
fn the_weight_floor_keeps_a_downvoted_memory_above_dormancy() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..100 {
        record_feedback(&conn, &id, FeedbackSignal::Unhelpful, None).unwrap();
    }

    let status: String = conn
        .query_row(
            "SELECT status FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    // base_weight floors at 0.1, which is above VITALITY_FLOOR of 0.05, so it
    // stays active — pinning that rather than assuming it flips.
    assert_eq!(status, "active");
}

#[test]
fn feedback_never_touches_access_count() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();
    record_feedback(&conn, &id, FeedbackSignal::Unhelpful, Some("a query")).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT access_count FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "access_count feeds sqrt(n+1); a negative access has no meaning"
    );
}

#[test]
fn an_unknown_memory_reports_not_found() {
    let db = Database::open_in_memory().unwrap();
    assert!(
        record_feedback(&db.conn(), "mem_nope", FeedbackSignal::Helpful, None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn deleting_a_memory_removes_its_feedback() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("a query")).unwrap();
    assert_eq!(feedback_rows(&conn, &id), 1);

    queries::delete_memory(&conn, &id).unwrap();

    // There is no foreign key here — the reference omits it so sync can deliver
    // rows out of order — so this relies on delete_memory cleaning up itself.
    assert_eq!(feedback_rows(&conn, &id), 0);
}

#[test]
fn tokenize_drops_single_characters_and_deduplicates() {
    assert_eq!(tokenize_query("a the THE cat"), vec!["cat", "the"]);
    assert!(tokenize_query("? ! .").is_empty());
}
