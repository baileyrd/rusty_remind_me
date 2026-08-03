//! Vault digest — a periodic "what's happened lately" summary.
//!
//! # Sensitive memories are excluded with no override
//!
//! Unlike search and list, this has no `include_sensitive`. A digest is
//! precisely the ambient, passive surface the flag exists to keep sensitive
//! content off: it is often delivered on a schedule rather than read in
//! response to a specific question, so there is no per-call caller intent to
//! opt back in against. The reference makes the same call for the same reason.
//!
//! # Sections that are not here yet
//!
//! The reference's digest has five sections. Two of them —
//! upcoming/overdue reminders, and sync status — read from subsystems this
//! crate does not have yet (issues #116 and #114 respectively).
//!
//! They are modelled as `Option` and **omitted** rather than rendered empty.
//! A "Reminders: none" line on a build with no reminders subsystem would say
//! something false: it reads as "you have nothing due", when the truth is
//! "nothing here can tell". Omitting the section is the honest shape, and
//! filling it in is a one-line change for whichever issue lands first.

use crate::models::{DigestData, DigestRecentMemory};
use crate::vitality::build_vitality_report;
use rusqlite::{params, Connection, Result};

/// Days back that count as "recent" by default.
pub const DEFAULT_SINCE_DAYS: i64 = 7;
/// Cap on the memories listed. `recent_total` carries the true count, so a
/// busy week reads as "20 of 340" rather than silently looking like 20.
pub const MAX_RECENT_MEMORIES: usize = 20;

pub const DIGEST_SINCE_DAYS_MIN: i64 = 1;
pub const DIGEST_SINCE_DAYS_MAX: i64 = 365;

/// Assemble the digest's underlying data.
pub fn build_digest(conn: &Connection, since_days: i64) -> Result<DigestData> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(since_days)).to_rfc3339();

    let mut stmt = conn.prepare(
        "SELECT id, content, category, created_at
           FROM memories
          WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?
          ORDER BY created_at DESC
          LIMIT ?",
    )?;
    let recent_memories: Vec<DigestRecentMemory> = stmt
        .query_map(params![cutoff, MAX_RECENT_MEMORIES as i64], |r| {
            Ok(DigestRecentMemory {
                id: r.get(0)?,
                content: r.get(1)?,
                category: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    drop(stmt);

    // Counted rather than taken from the capped list, so the cap is visible
    // rather than silently making a busy week look like a quiet one.
    let recent_total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories
          WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?",
        params![cutoff],
        |r| r.get(0),
    )?;

    Ok(DigestData {
        generated_at: chrono::Utc::now().to_rfc3339(),
        since_days,
        recent_memories,
        recent_total,
        vitality: build_vitality_report(conn)?,
        reminders_upcoming: None,
        reminders_overdue: None,
        sync: None,
    })
}

/// Render a digest as Markdown.
///
/// Sections with no data still appear with an explicit "nothing" line —
/// "no new memories this week" is information. Sections whose *subsystem* is
/// absent are omitted entirely, because there is nothing to report either way
/// and a false "none" would be worse than silence.
pub fn render_markdown(data: &DigestData) -> String {
    let mut out = String::from("# Vault digest\n\n");
    out.push_str(&format!(
        "_Generated {} · covering the last {} day{}_\n\n",
        data.generated_at,
        data.since_days,
        if data.since_days == 1 { "" } else { "s" }
    ));

    out.push_str("## Recent memories\n\n");
    if data.recent_memories.is_empty() {
        out.push_str("_Nothing new in this window._\n\n");
    } else {
        if data.recent_total > data.recent_memories.len() as i64 {
            out.push_str(&format!(
                "_Showing {} of {}._\n\n",
                data.recent_memories.len(),
                data.recent_total
            ));
        }
        for memory in &data.recent_memories {
            let mut snippet: String = memory.content.chars().take(120).collect();
            if memory.content.chars().count() > 120 {
                snippet.push('…');
            }
            out.push_str(&format!(
                "- **{}** ({}) — {}\n",
                memory.id, memory.category, snippet
            ));
        }
        out.push('\n');
    }

    let vitality = &data.vitality;
    out.push_str("## Vitality\n\n");
    out.push_str(&format!(
        "- Vault health: {}\n- Active: {} · Dormant: {}\n- Average vitality: {:.3}\n\n",
        vitality.vault_health_score,
        vitality.active_count,
        vitality.dormant_count,
        vitality.average_vitality
    ));

    if let Some(upcoming) = &data.reminders_upcoming {
        out.push_str("## Reminders\n\n");
        if upcoming.is_empty() {
            out.push_str("_Nothing upcoming._\n\n");
        } else {
            for line in upcoming {
                out.push_str(&format!("- {}\n", line));
            }
            out.push('\n');
        }
    }

    if let Some(sync) = &data.sync {
        out.push_str(&format!("## Sync\n\n{}\n", sync));
    }

    out
}
