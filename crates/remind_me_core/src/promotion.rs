//! The refinement ladder: capture → fact → scenario → persona.
//!
//! # What was missing
//!
//! The rungs already existed; the ladder did not. `capture.rs` holds raw
//! dialog, `remind_me_decompose` turns one into atomic facts, `wiki.rs`
//! compiles topic pages and `consolidation.rs` merges near-duplicates. But
//! every promotion was agent-initiated and one-shot. Nothing walked the store
//! asking which captures were never decomposed, which facts had accumulated
//! enough to deserve a scenario, or which scenarios were stable enough to say
//! something durable about the user. `UndecomposedCapture` existed — the
//! backlog was visible and simply never worked.
//!
//! This module is the missing walk. It does not distil anything itself: it
//! reports what is *ready* to move up ([`promotion_candidates`]) and accepts
//! the distillation back ([`promote`]), which is how `remind_me_decompose`
//! already works. No LLM is called from here.
//!
//! # Why rung 1 has candidates but no promotion
//!
//! `capture → fact` promotion **is** `remind_me_decompose`, which already
//! exists, already links `source_capture_id`, already applies entity mentions
//! and already supersedes contradicted facts. A second write path to the same
//! rung would be two implementations of one operation, and they would drift.
//! So [`promotion_candidates`] unifies the backlog view across all three rungs
//! and [`promote`] refuses rung 1, naming the tool that does it.
//!
//! # Provenance is not optional
//!
//! Every promoted artifact records which memories it came from, in the
//! target-only `promotions` table. Without it a persona statement is
//! unfalsifiable — you cannot ask what it was derived from, so you cannot tell
//! whether it is still true. The table is indexed both ways, so
//! [`provenance`] answers "what did this come from" and "what was built on
//! this" at the same cost.
//!
//! That is also what makes **demotion** automatic. [`persona`] excludes any
//! statement whose sources have all been superseded or deleted, so a fact
//! contradicted through `supersede_contradicting_facts` silently withdraws the
//! persona built on it, with no background job and no second opinion about
//! what "still true" means.
//!
//! # Not in the generated schema
//!
//! `promotions` is target-only, created by [`ensure_schema`] the way
//! `vectors::ensure_schema` creates `vec_embeddings` and
//! `archive::ensure_schema` creates its own. `db/schema_tables.sql` is
//! generated verbatim from `remind_me` and is not this crate's to extend.

use crate::models::{
    Memory, PersonaStatement, PromoteInput, PromotionCandidate, PromotionResult, Provenance, Rung,
    FACT_CATEGORY, PERSONA_CATEGORY, SCENARIO_CATEGORY,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use rusqlite::{params, params_from_iter, Connection, Result as SqlResult};

/// `source` promoted artifacts are stored under.
pub const PROMOTION_SOURCE: &str = "promotion";

/// Fewest facts sharing an entity before a scenario is worth proposing.
///
/// Two facts about the same entity is a coincidence; the threshold is what
/// stops the candidate list being one entry per entity in the store from the
/// moment anything is decomposed.
pub const MIN_SCENARIO_FACTS: usize = 3;

/// Vitality a scenario must still hold to be persona material.
///
/// The persona rung is meant to be *stable*, so the gate is the decay model
/// already in use rather than a new stability counter: a scenario that has not
/// been touched since it was written falls below this on its own.
pub const PERSONA_VITALITY_FLOOR: f64 = 0.5;

/// Longest snippet carried in a candidate listing.
const SNIPPET_CHARS: usize = 200;

/// Create this crate's own promotion-provenance table, if absent.
///
/// Called from [`crate::db::schema::initialize_schema`] after the generated
/// schema is applied. Created unconditionally: an empty table costs nothing,
/// and a lazily-created one would make every read path tolerate its absence.
///
/// No foreign keys. A promoted memory can be deleted through the ordinary
/// delete path, and a cascade would erase the provenance that says what it
/// *was* derived from — which is exactly the record needed to explain why a
/// persona statement vanished.
pub fn ensure_schema(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS promotions (
            promoted_id TEXT NOT NULL,
            source_id   TEXT NOT NULL,
            rung        TEXT NOT NULL,
            promoted_at TEXT NOT NULL,
            PRIMARY KEY (promoted_id, source_id)
         );
         CREATE INDEX IF NOT EXISTS idx_promotions_source
            ON promotions(source_id);",
    )
}

/// Why a promotion could not be made.
#[derive(Debug)]
pub enum PromotionError {
    Db(rusqlite::Error),
    /// Rung 1 is `remind_me_decompose`'s job.
    UseDecompose,
    /// A promotion with no sources has no provenance, so it could never be
    /// checked or demoted.
    NoSources,
    /// A named source does not exist, or is deleted/superseded.
    UnusableSource(String),
    /// A sensitive memory cannot become part of a persona.
    SensitiveSource(String),
    /// The distillation itself was empty.
    EmptyContent,
}

impl std::fmt::Display for PromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{}", e),
            Self::UseDecompose => write!(
                f,
                "capture_to_fact promotion is remind_me_decompose — it already links \
                 source_capture_id, applies entity mentions and supersedes contradicted \
                 facts. Use that tool; this one covers the rungs above it."
            ),
            Self::NoSources => write!(
                f,
                "a promotion needs at least one source memory: without provenance the \
                 result cannot be checked against what it came from, or demoted when \
                 that changes"
            ),
            Self::UnusableSource(id) => write!(
                f,
                "source {:?} does not exist, or has been deleted or superseded",
                id
            ),
            Self::SensitiveSource(id) => write!(
                f,
                "source {:?} is marked sensitive, so it cannot be promoted into a \
                 persona — a persona is an ambient surface, like a digest",
                id
            ),
            Self::EmptyContent => write!(f, "the promoted content is empty"),
        }
    }
}

impl std::error::Error for PromotionError {}

impl From<rusqlite::Error> for PromotionError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

fn snippet(content: &str) -> String {
    content.chars().take(SNIPPET_CHARS).collect()
}

/// Captures nothing has decomposed yet.
///
/// The predicate is `capture::undecomposed_batch`'s, deliberately: one
/// definition of "not yet decomposed", so this listing and that tool cannot
/// disagree about whether a capture is done.
fn capture_candidates(conn: &Connection, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content
           FROM memories m
          WHERE m.capture_id IS NOT NULL
            AND m.source_capture_id IS NULL
            AND m.deleted_at IS NULL
            AND m.category = 'dialog'
            AND NOT EXISTS (
                SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
            )
          ORDER BY m.created_at DESC
          LIMIT ?",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        let id: String = r.get(0)?;
        let content: String = r.get(1)?;
        Ok(PromotionCandidate {
            rung: Rung::CaptureToFact,
            source_ids: vec![id],
            snippet: snippet(&content),
            reason: "captured but never decomposed into facts".to_string(),
            grouped_by: None,
        })
    })?;
    rows.collect()
}

/// Facts clustered by a shared entity, where no scenario has been built from
/// them yet.
///
/// Grouping by entity rather than by embedding similarity on purpose:
/// `consolidation.rs` already clusters on cosine distance to find things that
/// are *the same*, and this rung wants things that are *related but distinct*.
/// The entity graph is the existing structure that expresses that.
fn scenario_candidates(conn: &Connection, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.name, group_concat(m.id), count(*) AS n
           FROM memories m
           JOIN memory_entities me ON me.memory_id = m.id
           JOIN entities e ON e.id = me.entity_id
          WHERE m.category = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
            )
          GROUP BY e.id
         HAVING n >= ?
          ORDER BY n DESC
          LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![
            FACT_CATEGORY,
            Rung::FactToScenario.as_str(),
            MIN_SCENARIO_FACTS as i64,
            limit as i64
        ],
        |r| {
            let name: String = r.get(1)?;
            let ids: String = r.get(2)?;
            let count: i64 = r.get(3)?;
            let source_ids: Vec<String> = ids.split(',').map(str::to_string).collect();
            Ok(PromotionCandidate {
                rung: Rung::FactToScenario,
                snippet: format!("{} facts mentioning {}", count, name),
                reason: format!(
                    "{} un-promoted facts share the entity {:?} — enough for a scenario",
                    count, name
                ),
                grouped_by: Some(name),
                source_ids,
            })
        },
    )?;
    rows.collect()
}

/// Scenarios stable enough to say something durable, not yet in a persona.
fn persona_candidates(conn: &Connection, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content, m.vitality
           FROM memories m
          WHERE m.category = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL
            AND m.sensitive = 0
            AND m.vitality >= ?
            AND NOT EXISTS (
                SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
            )
          ORDER BY m.vitality DESC
          LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![
            SCENARIO_CATEGORY,
            PERSONA_VITALITY_FLOOR,
            Rung::ScenarioToPersona.as_str(),
            limit as i64
        ],
        |r| {
            let id: String = r.get(0)?;
            let content: String = r.get(1)?;
            let vitality: f64 = r.get(2)?;
            Ok(PromotionCandidate {
                rung: Rung::ScenarioToPersona,
                source_ids: vec![id],
                snippet: snippet(&content),
                reason: format!(
                    "scenario still at vitality {:.2}, above the {:.2} persona floor",
                    vitality, PERSONA_VITALITY_FLOOR
                ),
                grouped_by: None,
            })
        },
    )?;
    rows.collect()
}

/// What is ready to move up a rung.
///
/// Every rung's query excludes what has already been promoted, so running this
/// twice against unchanged data returns the same list and promoting from it
/// twice is impossible — idempotency comes from the candidate query, not from
/// the caller remembering.
pub fn promotion_candidates(
    conn: &Connection,
    rung: Rung,
    limit: usize,
) -> SqlResult<Vec<PromotionCandidate>> {
    let limit = limit.clamp(1, 100);
    match rung {
        Rung::CaptureToFact => capture_candidates(conn, limit),
        Rung::FactToScenario => scenario_candidates(conn, limit),
        Rung::ScenarioToPersona => persona_candidates(conn, limit),
    }
}

/// One source memory, as far as promotion cares.
struct SourceRow {
    sensitive: bool,
}

fn load_source(conn: &Connection, id: &str) -> SqlResult<Option<SourceRow>> {
    conn.query_row(
        "SELECT sensitive FROM memories
          WHERE id = ? AND deleted_at IS NULL AND superseded_by IS NULL",
        params![id],
        |r| {
            Ok(SourceRow {
                sensitive: r.get(0)?,
            })
        },
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
    })
}

/// Accept a distillation and record what it was built from.
pub fn promote(conn: &Connection, input: &PromoteInput) -> Result<PromotionResult, PromotionError> {
    if input.rung == Rung::CaptureToFact {
        return Err(PromotionError::UseDecompose);
    }
    if input.content.trim().is_empty() {
        return Err(PromotionError::EmptyContent);
    }
    if input.source_ids.is_empty() {
        return Err(PromotionError::NoSources);
    }

    let category = match input.rung {
        Rung::FactToScenario => SCENARIO_CATEGORY,
        Rung::ScenarioToPersona => PERSONA_CATEGORY,
        Rung::CaptureToFact => unreachable!("refused above"),
    };

    // Validated before anything is written, so a promotion naming one bad
    // source does not leave a partially-linked artifact behind.
    for id in &input.source_ids {
        let source =
            load_source(conn, id)?.ok_or_else(|| PromotionError::UnusableSource(id.clone()))?;
        if source.sensitive && input.rung == Rung::ScenarioToPersona {
            return Err(PromotionError::SensitiveSource(id.clone()));
        }
    }

    let now_iso = Utc::now().to_rfc3339();
    let now = Utc::now();
    let promoted_id = format!("mem_{}", uuid::Uuid::new_v4().simple());

    let metadata = serde_json::json!({
        "rung": input.rung.as_str(),
        "promoted_from": input.source_ids,
    });

    let decay_rate = get_decay_rate(category);
    let base_weight = get_type_prior(category) * get_source_prior(PROMOTION_SOURCE);
    let vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata,
            created_at, updated_at, decay_rate, vitality, base_weight,
            access_count, accessed_at
         ) VALUES (?, ?, ?, '[]', ?, ?, ?, ?, ?, ?, ?, 0, ?)",
        params![
            promoted_id,
            input.content,
            category,
            PROMOTION_SOURCE,
            metadata.to_string(),
            now_iso,
            now_iso,
            decay_rate,
            vitality,
            base_weight,
            now_iso,
        ],
    )?;

    for source_id in &input.source_ids {
        conn.execute(
            "INSERT OR IGNORE INTO promotions (promoted_id, source_id, rung, promoted_at)
             VALUES (?, ?, ?, ?)",
            params![promoted_id, source_id, input.rung.as_str(), now_iso],
        )?;
    }

    Ok(PromotionResult {
        promoted_id,
        rung: input.rung,
        category: category.to_string(),
        source_ids: input.source_ids.clone(),
    })
}

/// What a memory was built from, and what was built on it.
///
/// Both directions from one indexed table, so "explain this persona statement"
/// and "what does this fact still support" cost the same.
pub fn provenance(conn: &Connection, memory_id: &str) -> SqlResult<Provenance> {
    let mut up = conn.prepare("SELECT source_id FROM promotions WHERE promoted_id = ?")?;
    let sources: Vec<String> = up
        .query_map(params![memory_id], |r| r.get(0))?
        .collect::<SqlResult<_>>()?;

    let mut down = conn.prepare("SELECT promoted_id FROM promotions WHERE source_id = ?")?;
    let derived: Vec<String> = down
        .query_map(params![memory_id], |r| r.get(0))?
        .collect::<SqlResult<_>>()?;

    Ok(Provenance {
        memory_id: memory_id.to_string(),
        sources,
        derived,
    })
}

/// How many of a promoted artifact's sources are still standing.
fn surviving_sources(conn: &Connection, promoted_id: &str) -> SqlResult<usize> {
    let count: i64 = conn.query_row(
        "SELECT count(*)
           FROM promotions p
           JOIN memories m ON m.id = p.source_id
          WHERE p.promoted_id = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL",
        params![promoted_id],
        |r| r.get(0),
    )?;
    Ok(count as usize)
}

/// The current persona: durable statements whose grounds still hold.
///
/// A statement every one of whose sources has been superseded or deleted is
/// **omitted rather than deleted**. Demotion is a read-time judgement, so a
/// fact restored (or a supersession undone) brings its persona statement back
/// without anything having to notice; deleting the row would make that
/// one-way. The row also stays as the record of what was once believed and
/// why, which is the thing provenance exists to preserve.
///
/// Sensitive statements are excluded with no override, matching
/// [`crate::digest`]: a persona is an ambient surface, assembled to be
/// injected rather than asked for, so there is no per-call intent to opt back
/// in against.
pub fn persona(conn: &Connection) -> SqlResult<Vec<PersonaStatement>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, vitality, created_at
           FROM memories
          WHERE category = ?
            AND deleted_at IS NULL
            AND superseded_by IS NULL
            AND sensitive = 0
          ORDER BY vitality DESC",
    )?;
    let rows: Vec<(String, String, f64, String)> = stmt
        .query_map(params![PERSONA_CATEGORY], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<SqlResult<_>>()?;

    let mut out = Vec::new();
    for (id, content, vitality, created_at) in rows {
        let surviving = surviving_sources(conn, &id)?;
        if surviving == 0 {
            continue;
        }
        out.push(PersonaStatement {
            id,
            content,
            vitality,
            created_at,
            surviving_sources: surviving,
        });
    }
    Ok(out)
}

/// Persona statements currently withheld, and why — the other half of
/// [`persona`].
///
/// Without this, a statement that quietly stopped appearing is indistinguishable
/// from one that was never written. A caller asking "why did the assistant stop
/// believing that" has somewhere to look.
pub fn demoted(conn: &Connection) -> SqlResult<Vec<PersonaStatement>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, vitality, created_at
           FROM memories
          WHERE category = ?
            AND deleted_at IS NULL
            AND superseded_by IS NULL
          ORDER BY created_at DESC",
    )?;
    let rows: Vec<(String, String, f64, String)> = stmt
        .query_map(params![PERSONA_CATEGORY], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<SqlResult<_>>()?;

    let mut out = Vec::new();
    for (id, content, vitality, created_at) in rows {
        if surviving_sources(conn, &id)? == 0 {
            out.push(PersonaStatement {
                id,
                content,
                vitality,
                created_at,
                surviving_sources: 0,
            });
        }
    }
    Ok(out)
}

/// Load the memories a set of ids names, in the order given, skipping any that
/// no longer resolve. Used by the tool layer to show a candidate's sources.
pub fn load_memories(conn: &Connection, ids: &[String]) -> SqlResult<Vec<Memory>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks = vec!["?"; ids.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE id IN ({})",
        crate::db::queries::MEMORY_COLUMNS,
        marks
    ))?;
    let rows = stmt.query_map(
        params_from_iter(ids.iter()),
        crate::db::queries::parse_memory_row,
    )?;
    rows.collect()
}
