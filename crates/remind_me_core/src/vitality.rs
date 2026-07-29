use crate::models::Memory;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const VITALITY_FLOOR: f64 = 0.05;
pub const BRIDGE_THRESHOLD: i64 = 10;
pub const BRIDGE_MULTIPLIER: f64 = 0.5;

pub fn get_decay_rate(category_or_type: &str) -> f64 {
    match category_or_type.to_lowercase().as_str() {
        "decision" => 0.02,
        "preference" => 0.03,
        "fact" => 0.05,
        "insight" => 0.07,
        "learning" => 0.08,
        "blocker" => 0.15,
        "action_item" => 0.20,
        _ => 0.10,
    }
}

pub fn get_type_prior(category_or_type: &str) -> f64 {
    match category_or_type.to_lowercase().as_str() {
        "decision" => 1.3,
        "blocker" => 1.2,
        "fact" | "insight" => 1.15,
        "preference" => 1.1,
        "learning" => 1.05,
        "action_item" | "unclassified" => 1.0,
        _ => 1.0,
    }
}

pub fn get_source_prior(source: &str) -> f64 {
    match source.to_lowercase().as_str() {
        "manual" => 1.0,
        "chat_import" => 0.85,
        "document_import" | "webhook" => 0.9,
        _ => 1.0,
    }
}

/// Calculate vitality score based on ACT-R formula:
/// vitality = base_weight * (access_count + 1)^0.5 * exp(-decay_rate * days_since_last_access)
pub fn calculate_vitality(
    base_weight: f64,
    access_count: i64,
    decay_rate: f64,
    accessed_at_iso: &str,
    now: DateTime<Utc>,
) -> f64 {
    let effective_decay = if access_count >= BRIDGE_THRESHOLD {
        decay_rate * BRIDGE_MULTIPLIER
    } else {
        decay_rate
    };

    let last_access = DateTime::parse_from_rfc3339(accessed_at_iso)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);

    let seconds = (now - last_access).num_seconds().max(0);
    let days = seconds as f64 / 86400.0;

    let frequency_boost = ((access_count as f64) + 1.0).sqrt();
    let decay_factor = (-effective_decay * days).exp();

    base_weight * frequency_boost * decay_factor
}

/// Fractional adjustment one feedback event applies to `base_weight`.
pub const FEEDBACK_MAGNITUDE: f64 = 0.15;
/// Ceiling and floor `base_weight` is clamped to, so repeated feedback on one
/// memory cannot run away in either direction.
pub const BASE_WEIGHT_MAX: f64 = 3.0;
pub const BASE_WEIGHT_MIN: f64 = 0.1;

/// A signed retrieval-quality signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedbackSignal {
    Helpful,
    Unhelpful,
}

/// Coarse tokenisation for clustering queries by similarity.
///
/// Lowercase, alphanumeric, single characters dropped, de-duplicated and
/// sorted. Deliberately crude — no stemming, stopwords or embeddings — because
/// it only has to separate "close enough to be the same question" from "a
/// different question", and must behave identically with or without an embedder
/// configured.
pub fn tokenize_query(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.chars().count() > 1)
        .map(|t| t.to_string())
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Record helpful/unhelpful feedback. Returns the memory's vitality, or `None`
/// if no such memory exists.
///
/// Two modes, chosen by whether `query` is supplied — this is the reference's
/// gap #6 design, not an embellishment:
///
/// - **No query:** a *global* judgement. `base_weight` is scaled up or down by
///   [`FEEDBACK_MAGNITUDE`], clamped, and vitality is recomputed. Every future
///   search sees it.
/// - **With a query:** *contextual*. The event is logged to `memory_feedback`
///   and `base_weight` is left alone, because a memory can be a poor answer to
///   one question and the right answer to another — demoting it globally would
///   punish the second for the first's feedback.
///
/// `access_count` is untouched in both modes. It feeds `sqrt(access_count + 1)`,
/// where a "negative access" has no meaning.
pub fn record_feedback(
    conn: &Connection,
    memory_id: &str,
    signal: FeedbackSignal,
    query: Option<&str>,
) -> Result<Option<f64>> {
    let mut stmt = conn.prepare(
        // decay_rate is deliberately not selected: at zero elapsed days the
        // decay factor is exp(0) = 1, so it drops out of the snapshot entirely.
        "SELECT access_count, base_weight, vitality FROM memories
         WHERE id = ? AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query([memory_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let access_count: i64 = row.get(0)?;
    let base_weight: f64 = row.get(1)?;
    let vitality: f64 = row.get(2)?;
    drop(rows);
    drop(stmt);

    if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
        conn.execute(
            "INSERT INTO memory_feedback
                (id, memory_id, query, query_tokens, signal, magnitude, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                format!("fb_{}", uuid::Uuid::new_v4().simple()),
                memory_id,
                query,
                tokenize_query(query).join(" "),
                match signal {
                    FeedbackSignal::Helpful => "helpful",
                    FeedbackSignal::Unhelpful => "unhelpful",
                },
                FEEDBACK_MAGNITUDE,
                Utc::now().to_rfc3339(),
            ],
        )?;
        return Ok(Some(vitality));
    }

    let new_base_weight = match signal {
        FeedbackSignal::Helpful => (base_weight * (1.0 + FEEDBACK_MAGNITUDE)).min(BASE_WEIGHT_MAX),
        FeedbackSignal::Unhelpful => {
            (base_weight * (1.0 - FEEDBACK_MAGNITUDE)).max(BASE_WEIGHT_MIN)
        }
    };

    // Snapshot recompute with zero elapsed days, matching how `add_memory` seeds
    // the column: the stored value is a write-time snapshot and
    // `effective_vitality` applies decay on read.
    let new_vitality = new_base_weight * ((access_count as f64) + 1.0).sqrt();

    conn.execute(
        "UPDATE memories SET base_weight = ?, vitality = ?, status = ? WHERE id = ?",
        rusqlite::params![
            new_base_weight,
            new_vitality,
            if is_dormant(new_vitality) {
                "dormant"
            } else {
                "active"
            },
            memory_id
        ],
    )?;

    Ok(Some(new_vitality))
}

/// A memory's vitality *right now*, with real elapsed-days decay applied.
///
/// The stored `vitality` column is a write-time snapshot: [`add_memory`] computes
/// it with `access_count = 0` and zero elapsed days, so it never decays on its
/// own. Anything that treats the stored value as current will consider a
/// year-old memory just as vital as one written this morning.
///
/// Reads the real `base_weight` column, which the schema ladder added. An
/// earlier version of this function used the stored `vitality` as a stand-in,
/// which was exact only while nothing ever wrote to `vitality` after insert —
/// a fragile arrangement that would have double-counted the frequency boost the
/// moment access tracking landed. That hazard is now gone.
pub fn effective_vitality(memory: &Memory, now: DateTime<Utc>) -> f64 {
    calculate_vitality(
        memory.base_weight,
        memory.access_count,
        memory.decay_rate,
        &memory.accessed_at,
        now,
    )
}

/// Whether a memory has decayed below [`VITALITY_FLOOR`].
///
/// Takes an *effective* vitality — see [`effective_vitality`]. Passing the raw
/// stored column would mean nothing is ever dormant.
pub fn is_dormant(vitality: f64) -> bool {
    vitality < VITALITY_FLOOR
}

/// Vault health snapshot.
///
/// Field names mirror the reference's `build_vitality_report` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VitalityReport {
    pub total_memories: usize,
    pub active_count: usize,
    pub dormant_count: usize,
    pub average_vitality: f64,
    /// Percentage of memories that are still active, e.g. `"82%"`.
    pub vault_health_score: String,
    /// Counts per category. The reference groups by `memory_type`; this crate
    /// has no such column and `category` fills that role — it is what
    /// [`get_decay_rate`] and [`get_type_prior`] key off.
    pub decay_distribution: BTreeMap<String, i64>,
    pub vitality_buckets: BTreeMap<String, usize>,
}

/// Bucket edges, low inclusive and high exclusive.
///
/// The top bucket is deliberately **open-ended**. An accessed memory exceeds
/// 1.0 — a single access gives `sqrt(2) ≈ 1.41` — so a closed top bucket would
/// drop those rows and the counts would not sum to the total. That is the
/// reference's `DI-04` fix, ported here rather than the pre-fix behavior.
const BUCKET_RANGES: [(&str, f64, f64); 4] = [
    ("0.00-0.05", 0.0, 0.05),
    ("0.05-0.25", 0.05, 0.25),
    ("0.25-0.50", 0.25, 0.50),
    ("0.50-0.75", 0.50, 0.75),
];
const TOP_BUCKET: &str = "0.75+";

fn bucket_for(vitality: f64) -> &'static str {
    for (label, low, high) in BUCKET_RANGES {
        if vitality >= low && vitality < high {
            return label;
        }
    }
    TOP_BUCKET
}

/// Build the vault vitality report.
///
/// Decay is applied at report time via [`effective_vitality`], and dormancy is
/// derived from that rather than from the stored column.
pub fn build_vitality_report(conn: &Connection) -> Result<VitalityReport> {
    // `deleted_at IS NULL` is a no-op while deletes are hard, but keeps the
    // report correct once sync introduces tombstones. The reference omits this
    // filter and would count tombstoned rows.
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE deleted_at IS NULL",
        crate::db::queries::MEMORY_COLUMNS
    ))?;
    let rows = stmt.query_map([], crate::db::queries::parse_memory_row)?;

    let now = Utc::now();
    let mut vitalities = Vec::new();
    let mut decay_distribution: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        let memory = row?;
        *decay_distribution
            .entry(memory.category.clone())
            .or_insert(0) += 1;
        vitalities.push(effective_vitality(&memory, now));
    }

    let total = vitalities.len();
    let dormant_count = vitalities.iter().filter(|v| is_dormant(**v)).count();
    let active_count = total - dormant_count;

    let average_vitality = if total == 0 {
        0.0
    } else {
        let mean = vitalities.iter().sum::<f64>() / total as f64;
        (mean * 100.0).round() / 100.0
    };

    // Seed every label so absent buckets report 0 rather than vanishing.
    let mut vitality_buckets: BTreeMap<String, usize> = BUCKET_RANGES
        .iter()
        .map(|(label, _, _)| ((*label).to_string(), 0))
        .collect();
    vitality_buckets.insert(TOP_BUCKET.to_string(), 0);
    for v in &vitalities {
        *vitality_buckets.get_mut(bucket_for(*v)).unwrap() += 1;
    }

    let health_pct = if total == 0 {
        0
    } else {
        (active_count as f64 / total as f64 * 100.0).round() as i64
    };

    Ok(VitalityReport {
        total_memories: total,
        active_count,
        dormant_count,
        average_vitality,
        vault_health_score: format!("{}%", health_pct),
        decay_distribution,
        vitality_buckets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_labels_sort_in_numeric_order() {
        // The report is a BTreeMap, so labels are emitted in lexicographic
        // order. These particular labels happen to sort numerically too, which
        // is what makes a plain BTreeMap adequate here.
        let mut labels: Vec<&str> = BUCKET_RANGES.iter().map(|(l, _, _)| *l).collect();
        labels.push(TOP_BUCKET);
        let mut sorted = labels.clone();
        sorted.sort();
        assert_eq!(labels, sorted);
    }

    #[test]
    fn top_bucket_catches_values_above_one() {
        // One access gives sqrt(2) ~= 1.41; a closed top bucket would lose it.
        assert_eq!(bucket_for(1.41), "0.75+");
        assert_eq!(bucket_for(0.75), "0.75+");
        assert_eq!(bucket_for(0.74), "0.50-0.75");
        assert_eq!(bucket_for(0.0), "0.00-0.05");
    }

    #[test]
    fn test_vitality_calculation() {
        let now = Utc::now();
        let now_iso = now.to_rfc3339();
        let vit = calculate_vitality(1.0, 0, 0.10, &now_iso, now);
        assert!((vit - 1.0).abs() < 1e-4);
    }
}
