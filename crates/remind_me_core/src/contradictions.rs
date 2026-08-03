//! Free-text contradiction candidates.
//!
//! [`crate::entity`]'s write path already auto-supersedes a memory whenever a
//! new triple shares an existing one's (subject, predicate) but asserts a
//! different object. That mechanism only fires on exact triple structure. It
//! says nothing about two pieces of prose that conflict without either
//! carrying a formal triple — "I moved to Boston" against "I live in Seattle",
//! written as plain text. This surfaces exactly that gap.
//!
//! Read-only, and it returns pairs that *might* conflict rather than pairs
//! that do. Prose comparison is inherently less certain than exact triple
//! matching, so the judgment stays with the calling session; most pairs turn
//! out merely topically similar. There is deliberately no apply tool — a
//! confirmed contradiction is fixed with the existing `remind_me_update`,
//! `remind_me_delete`, or an `remind_me_add` carrying an explicit triple.
//!
//! # Why the fan-out cap exists
//!
//! The comparison space is bounded by the entity graph rather than all-pairs:
//! two memories are only worth comparing if they mention an entity in common.
//! That alone is not enough. A broadly-mentioned entity — a person, or a
//! project with hundreds of memories about it — makes "shares an entity" stop
//! meaning anything, and the pair count from one such entity is quadratic in
//! its mention count.
//!
//! The reference measured this on a real vault: a single 745-mention project
//! entity produced 277,140 of 372,750 total candidates — 74% of the queue —
//! before the cap existed. Entities mentioned by more than
//! [`MAX_ENTITY_FANOUT`] memories are excluded from the join entirely, on both
//! sides, since either side of the self-join can land on the hub entity.
//!
//! This is not an optimisation to add later. Without it the queue is
//! dominated by pairs whose only relationship is naming the same project, and
//! the tool is unusable on exactly the vaults that need it most.

use crate::models::{ContradictionCandidate, ContradictionCandidatesResult, ContradictionSide};
use rusqlite::{params, Connection, Result};

/// Entities mentioned by more memories than this are excluded from the pairing
/// join.
///
/// Chosen empirically by the reference as the fan-out above which pairs stop
/// reading as plausible candidates and start reading as "these two both
/// mention the same project".
pub const MAX_ENTITY_FANOUT: i64 = 20;

/// Characters of each side's content returned, enough to judge a pair without
/// returning two whole documents per row.
const SNIPPET_CHARS: usize = 500;

/// Pairs of live, non-dialog memories sharing a low-fan-out entity, excluding
/// anything the exact-triple mechanism already covers.
///
/// The triple exclusion is subtler than it looks. A pair where both sides
/// share a normalised (subject, predicate) but differ in object *cannot* be
/// observed here: the moment the second was written, the supersession check
/// would have set `superseded_by` on the first, and this query only considers
/// live rows. So excluding matching subject+predicate filters out same-object
/// verbatim restatements — not a contradiction worth flagging — rather than
/// defending against pairs that could otherwise slip through.
///
/// `lower`/`trim` approximates the entity-name normalisation rather than
/// reproducing it exactly. This only narrows a set for human review, so an
/// imprecise exclusion is a false negative — the caller recognises the pair
/// and skips it — not the correctness bug it would be in the write-path check.
fn pairs_sql() -> String {
    format!(
        "SELECT DISTINCT me1.memory_id AS id_a, me2.memory_id AS id_b
           FROM memory_entities me1
           JOIN memory_entities me2
             ON me2.entity_id = me1.entity_id AND me2.memory_id > me1.memory_id
           JOIN memories m1 ON m1.id = me1.memory_id
           JOIN memories m2 ON m2.id = me2.memory_id
           JOIN (
               SELECT entity_id, COUNT(*) AS mentions
                 FROM memory_entities
                GROUP BY entity_id
           ) fanout ON fanout.entity_id = me1.entity_id
          WHERE m1.superseded_by IS NULL AND m1.deleted_at IS NULL
            AND m2.superseded_by IS NULL AND m2.deleted_at IS NULL
            AND m1.category != 'dialog' AND m2.category != 'dialog'
            AND fanout.mentions <= {fanout}
            AND NOT (
                m1.subject IS NOT NULL AND m1.predicate IS NOT NULL
                AND m2.subject IS NOT NULL AND m2.predicate IS NOT NULL
                AND lower(trim(m1.subject)) = lower(trim(m2.subject))
                AND lower(trim(m1.predicate)) = lower(trim(m2.predicate))
            )",
        fanout = MAX_ENTITY_FANOUT
    )
}

fn side(conn: &Connection, memory_id: &str) -> Result<ContradictionSide> {
    conn.query_row(
        &format!(
            "SELECT id, substr(content, 1, {}) AS content_snippet, category,
                    memory_type, subject, predicate, object, created_at
               FROM memories WHERE id = ?",
            SNIPPET_CHARS
        ),
        params![memory_id],
        |r| {
            Ok(ContradictionSide {
                id: r.get(0)?,
                content_snippet: r.get(1)?,
                category: r.get(2)?,
                memory_type: r.get(3)?,
                subject: r.get(4)?,
                predicate: r.get(5)?,
                object: r.get(6)?,
                created_at: r.get(7)?,
            })
        },
    )
}

/// A batch of candidate pairs, plus the full backlog size.
pub fn candidates(conn: &Connection, limit: usize) -> Result<ContradictionCandidatesResult> {
    let pairs = pairs_sql();

    let total: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM ({}) p", pairs), [], |r| {
        r.get(0)
    })?;

    let mut stmt = conn.prepare(&format!(
        "SELECT id_a, id_b FROM ({}) ORDER BY id_a, id_b LIMIT ?",
        pairs
    ))?;
    let ids: Vec<(String, String)> = stmt
        .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>>>()?;
    drop(stmt);

    let mut candidates = Vec::with_capacity(ids.len());
    for (id_a, id_b) in ids {
        candidates.push(ContradictionCandidate {
            memory_a: side(conn, &id_a)?,
            memory_b: side(conn, &id_b)?,
        });
    }

    Ok(ContradictionCandidatesResult {
        candidates,
        total_candidates: total,
    })
}
