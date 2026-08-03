//! Coverage for `remind_me_contradiction_candidates` (gap T6, issue #110).
//!
//! The fan-out cap gets the most attention here, because it is the part that
//! is invisible on a small vault and decisive on a real one: without it a
//! single broadly-mentioned entity contributes a quadratic number of pairs
//! whose only relationship is naming the same project.

use remind_me_core::contradictions::{candidates, MAX_ENTITY_FANOUT};
use remind_me_core::db::queries;
use remind_me_core::{Database, EntityInput, MemoryAddInput};
use rusqlite::Connection;

fn add(
    conn: &Connection,
    content: &str,
    category: &str,
    entities: &[&str],
    triple: Option<(&str, &str, &str)>,
) -> String {
    let (subject, predicate, object) = match triple {
        Some((s, p, o)) => (
            Some(s.to_string()),
            Some(p.to_string()),
            Some(o.to_string()),
        ),
        None => (None, None, None),
    };
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: category.to_string(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject,
            predicate,
            object,
            entities: entities
                .iter()
                .map(|name| EntityInput {
                    name: name.to_string(),
                    kind: None,
                    aliases: vec![],
                })
                .collect(),
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

fn total(conn: &Connection) -> i64 {
    candidates(conn, 100).unwrap().total_candidates
}

#[test]
fn two_memories_sharing_an_entity_are_a_candidate_pair() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "I moved to Boston", "general", &["Boston"], None);
    add(&conn, "I live in Seattle now", "general", &["Boston"], None);

    let result = candidates(&conn, 20).unwrap();

    assert_eq!(result.total_candidates, 1);
    assert_eq!(result.candidates.len(), 1);
    // Both sides carry enough to judge without a second round trip — the whole
    // point is that the calling session reads them and decides.
    assert!(!result.candidates[0].memory_a.content_snippet.is_empty());
    assert!(!result.candidates[0].memory_b.content_snippet.is_empty());
}

#[test]
fn memories_sharing_no_entity_are_not_compared() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "I moved to Boston", "general", &["Boston"], None);
    add(&conn, "the build is green", "general", &["CI"], None);

    // All-pairs over a whole vault would be quadratic and mostly noise. The
    // entity graph is what bounds the comparison space.
    assert_eq!(total(&conn), 0);
}

#[test]
fn dialog_memories_are_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "I moved to Boston", "dialog", &["Boston"], None);
    add(&conn, "I live in Seattle", "dialog", &["Boston"], None);

    // A captured transcript's facts are meant to come out through decompose;
    // pairing raw dialog would flood the queue with conversational back-and-
    // forth that was never asserting anything.
    assert_eq!(total(&conn), 0);
}

#[test]
fn deleted_and_superseded_memories_are_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add(&conn, "I moved to Boston", "general", &["Boston"], None);
    let b = add(&conn, "I live in Seattle", "general", &["Boston"], None);
    assert_eq!(total(&conn), 1);

    conn.execute(
        "UPDATE memories SET deleted_at = '2026-01-01T00:00:00+00:00' WHERE id = ?",
        [&a],
    )
    .unwrap();
    assert_eq!(total(&conn), 0, "a tombstoned memory asserts nothing");

    conn.execute("UPDATE memories SET deleted_at = NULL WHERE id = ?", [&a])
        .unwrap();
    conn.execute(
        "UPDATE memories SET superseded_by = ? WHERE id = ?",
        [&a, &b],
    )
    .unwrap();
    assert_eq!(
        total(&conn),
        0,
        "a superseded memory has already been resolved"
    );
}

#[test]
fn a_pair_the_triple_mechanism_covers_is_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Same normalised subject and predicate. A *differing* object cannot be
    // observed here — the write path would have superseded the first the
    // moment the second landed — so this exclusion filters out same-object
    // verbatim restatements, which are not a contradiction worth flagging.
    add(
        &conn,
        "Bailey lives in Boston",
        "general",
        &["Bailey"],
        Some(("Bailey", "lives_in", "Boston")),
    );
    add(
        &conn,
        "  bailey   LIVES_IN Boston  ",
        "general",
        &["Bailey"],
        Some(("  Bailey  ", "LIVES_IN", "Boston")),
    );

    // Case- and whitespace-insensitive, so a restatement that differs only in
    // formatting is still recognised as covered.
    assert_eq!(total(&conn), 0);
}

#[test]
fn a_pair_with_only_one_side_carrying_a_triple_still_counts() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(
        &conn,
        "Bailey lives in Boston",
        "general",
        &["Bailey"],
        Some(("Bailey", "lives_in", "Boston")),
    );
    add(
        &conn,
        "Bailey moved to Seattle",
        "general",
        &["Bailey"],
        None,
    );

    // The exclusion needs BOTH sides to carry a matching triple. This is
    // exactly the gap the tool exists for: structured on one side, prose on
    // the other, so the exact-triple mechanism never fires.
    assert_eq!(total(&conn), 1);
}

// ---------------------------------------------------------------------------
// The fan-out cap
// ---------------------------------------------------------------------------

#[test]
fn a_broadly_mentioned_entity_is_excluded_entirely() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // One entity mentioned past the cap. Without the cap this alone would
    // contribute (n choose 2) pairs — on the reference author's vault, a
    // single 745-mention entity produced 74% of the entire queue.
    let over = MAX_ENTITY_FANOUT + 1;
    for i in 0..over {
        add(
            &conn,
            &format!("note {} about the big project", i),
            "general",
            &["BigProject"],
            None,
        );
    }

    assert_eq!(
        total(&conn),
        0,
        "past the cap, 'shares an entity' stops meaning anything"
    );
}

#[test]
fn an_entity_exactly_at_the_cap_still_pairs() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..MAX_ENTITY_FANOUT {
        add(
            &conn,
            &format!("note {} about the project", i),
            "general",
            &["Project"],
            None,
        );
    }

    // The predicate is `<=`. Pinning the boundary means a later `<` fails here
    // rather than silently shrinking every vault's queue by one entity's worth.
    let n = MAX_ENTITY_FANOUT;
    assert_eq!(total(&conn), n * (n - 1) / 2);
}

#[test]
fn the_cap_applies_to_both_sides_of_the_join() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // A hub entity over the cap, plus a narrow entity shared by two of the
    // same memories. The narrow pair must survive; nothing may come through on
    // the hub. Capping only one side of the self-join would let pairs in
    // whichever way round the ids happened to sort.
    for i in 0..(MAX_ENTITY_FANOUT + 1) {
        add(&conn, &format!("hub note {}", i), "general", &["Hub"], None);
    }
    add(&conn, "narrow one", "general", &["Narrow"], None);
    add(&conn, "narrow two", "general", &["Narrow"], None);

    assert_eq!(total(&conn), 1, "only the narrow pair");
}

#[test]
fn total_counts_past_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        add(&conn, &format!("note {}", i), "general", &["Topic"], None);
    }

    let result = candidates(&conn, 3).unwrap();

    // 5 memories on one entity is 10 pairs. The count tells a caller how much
    // is behind the page, so it must not be derived from the page.
    assert_eq!(result.candidates.len(), 3);
    assert_eq!(result.total_candidates, 10);
}

#[test]
fn an_empty_store_is_an_empty_batch() {
    let db = Database::open_in_memory().unwrap();

    let result = candidates(&db.conn(), 20).unwrap();

    assert!(result.candidates.is_empty());
    assert_eq!(result.total_candidates, 0);
}
