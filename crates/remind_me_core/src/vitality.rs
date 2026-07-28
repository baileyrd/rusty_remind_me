use chrono::{DateTime, Utc};

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
    last_accessed_at_iso: &str,
    now: DateTime<Utc>,
) -> f64 {
    let effective_decay = if access_count >= BRIDGE_THRESHOLD {
        decay_rate * BRIDGE_MULTIPLIER
    } else {
        decay_rate
    };

    let last_access = DateTime::parse_from_rfc3339(last_accessed_at_iso)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);

    let seconds = (now - last_access).num_seconds().max(0);
    let days = seconds as f64 / 86400.0;

    let frequency_boost = ((access_count as f64) + 1.0).sqrt();
    let decay_factor = (-effective_decay * days).exp();

    base_weight * frequency_boost * decay_factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vitality_calculation() {
        let now = Utc::now();
        let now_iso = now.to_rfc3339();
        let vit = calculate_vitality(1.0, 0, 0.10, &now_iso, now);
        assert!((vit - 1.0).abs() < 1e-4);
    }
}
