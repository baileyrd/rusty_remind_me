use crate::models::{MemorySearchResult, RetrievalStrategy};

pub const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, Copy)]
pub struct RrfWeights {
    pub w_keyword: f64,
    pub w_semantic: f64,
    pub w_recency: f64,
    pub w_vitality: f64,
    pub w_idf: f64,
}

impl Default for RrfWeights {
    fn default() -> Self {
        Self {
            w_keyword: 1.0,
            w_semantic: 1.0,
            w_recency: 1.0,
            w_vitality: 1.0,
            w_idf: 0.0,
        }
    }
}

pub fn looks_keyword_shaped(query: &str) -> bool {
    query.contains('"') || query.contains('*') || query.split_whitespace().count() <= 2
}

pub fn looks_semantic_shaped(query: &str) -> bool {
    query.split_whitespace().count() >= 6 || query.trim().ends_with('?')
}

pub fn looks_temporal_shaped(query: &str) -> bool {
    let lower = query.to_lowercase();
    let keywords = [
        "before",
        "after",
        "since",
        "until",
        "when",
        "during",
        "ago",
        "recently",
        "yesterday",
        "today",
        "tomorrow",
        "last week",
        "last month",
        "last year",
    ];
    keywords.iter().any(|kw| lower.contains(kw))
}

pub fn choose_rrf_weights(query: &str, strategy: RetrievalStrategy) -> RrfWeights {
    match strategy {
        RetrievalStrategy::Balanced => RrfWeights::default(),
        RetrievalStrategy::KeywordFavored => RrfWeights {
            w_keyword: 1.5,
            w_semantic: 0.5,
            w_recency: 1.0,
            w_vitality: 1.0,
            w_idf: 0.5,
        },
        RetrievalStrategy::SemanticFavored => RrfWeights {
            w_keyword: 0.5,
            w_semantic: 1.5,
            w_recency: 1.0,
            w_vitality: 1.0,
            w_idf: 0.0,
        },
        RetrievalStrategy::Auto => {
            let mut weights = RrfWeights::default();
            if looks_keyword_shaped(query) {
                weights.w_keyword = 1.4;
                weights.w_semantic = 0.6;
            } else if looks_semantic_shaped(query) {
                weights.w_keyword = 0.6;
                weights.w_semantic = 1.4;
            }
            if looks_temporal_shaped(query) {
                weights.w_recency *= 1.5;
            }
            weights
        }
    }
}

/// Perform Reciprocal Rank Fusion on candidates.
pub fn rank_rrf(
    candidates: Vec<MemorySearchResult>,
    weights: RrfWeights,
) -> Vec<MemorySearchResult> {
    if candidates.is_empty() {
        return candidates;
    }

    let penalty_rank = (candidates.len() + 1) as f64;

    let mut ranked = candidates;
    for (i, item) in ranked.iter_mut().enumerate() {
        let kw_rank = (i + 1) as f64;
        let vitality = item.memory.vitality;
        let vit_rank = if vitality > 0.0 { 1.0 } else { penalty_rank };

        let score =
            (weights.w_keyword / (RRF_K + kw_rank)) + (weights.w_vitality / (RRF_K + vit_rank));

        item.score = score;
        item.fts_score = Some(weights.w_keyword / (RRF_K + kw_rank));
        item.vitality_score = Some(weights.w_vitality / (RRF_K + vit_rank));
    }

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

pub fn trim_by_token_budget(
    results: Vec<MemorySearchResult>,
    token_budget: usize,
) -> Vec<MemorySearchResult> {
    if token_budget == 0 {
        return results;
    }

    let mut accum_tokens = 0;
    let mut trimmed = Vec::new();

    for res in results {
        let est_tokens = (res.memory.content.len() / 4).max(1);
        if accum_tokens + est_tokens > token_budget && !trimmed.is_empty() {
            break;
        }
        accum_tokens += est_tokens;
        trimmed.push(res);
    }

    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_choose_rrf_weights_auto() {
        let w = choose_rrf_weights("what did I do last week?", RetrievalStrategy::Auto);
        assert!(w.w_recency > 1.0);
    }
}
