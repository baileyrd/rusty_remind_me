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
