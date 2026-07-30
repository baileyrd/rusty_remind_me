use crate::models::{Memory, MemorySearchResult, RetrievalStrategy};
use std::collections::{HashMap, HashSet};

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

/// Fuse a keyword-ranked list and a semantic-ranked list via Reciprocal Rank
/// Fusion.
///
/// Each list is already ordered by its own relevance (FTS `bm25` for
/// `keyword`, cosine similarity for `semantic`) — fusion here works from
/// *list position*, not the underlying score value, which is the point of
/// RRF: it combines rankings from incomparable scales without having to
/// normalize either one.
///
/// A memory present in only one list is not penalized to zero on the
/// other — it gets a *penalty rank* one past that list's length, the same
/// treatment the vitality term already used for a dormant memory. Before
/// `#49`, `semantic` was always empty (nothing produced vectors), which is
/// exactly the single-list case this still handles: every candidate's
/// semantic rank was the constant penalty, so `w_semantic` contributed a
/// constant that dropped out of the ordering entirely — silently correct,
/// never exercised.
pub fn rank_rrf(
    keyword: Vec<Memory>,
    semantic: Vec<Memory>,
    weights: RrfWeights,
) -> Vec<MemorySearchResult> {
    let keyword_rank: HashMap<String, usize> = keyword
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();
    let semantic_rank: HashMap<String, usize> = semantic
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();
    let keyword_penalty = (keyword.len() + 1) as f64;
    let semantic_penalty = (semantic.len() + 1) as f64;
    // Distinct from "semantic search ran and found nothing at this rank":
    // an empty `semantic` means it never ran at all (no embedder configured
    // or reachable), so `vec_score` reports `None` rather than the same
    // constant for every result, which would carry no information but look
    // like it did.
    let semantic_ran = !semantic.is_empty();

    // Union, keyword-first, deduplicated by id, so a memory in both lists
    // appears once.
    let mut seen: HashSet<String> = HashSet::new();
    let union: Vec<Memory> = keyword
        .into_iter()
        .chain(semantic)
        .filter(|m| seen.insert(m.id.clone()))
        .collect();
    let vitality_penalty = (union.len() + 1) as f64;

    let mut ranked: Vec<MemorySearchResult> = union
        .into_iter()
        .map(|memory| {
            let kw_rank = keyword_rank
                .get(memory.id.as_str())
                .map(|&r| r as f64)
                .unwrap_or(keyword_penalty);
            let sem_rank = semantic_rank
                .get(memory.id.as_str())
                .map(|&r| r as f64)
                .unwrap_or(semantic_penalty);
            let vit_rank = if memory.vitality > 0.0 {
                1.0
            } else {
                vitality_penalty
            };

            let fts_score = weights.w_keyword / (RRF_K + kw_rank);
            let vec_score = weights.w_semantic / (RRF_K + sem_rank);
            let vitality_score = weights.w_vitality / (RRF_K + vit_rank);

            MemorySearchResult {
                memory,
                score: fts_score + if semantic_ran { vec_score } else { 0.0 } + vitality_score,
                fts_score: Some(fts_score),
                vec_score: semantic_ran.then_some(vec_score),
                vitality_score: Some(vitality_score),
            }
        })
        .collect();

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

    fn mem(id: &str, vitality: f64) -> Memory {
        Memory {
            id: id.to_string(),
            content: String::new(),
            category: "general".to_string(),
            tags: vec![],
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            created_at: String::new(),
            updated_at: String::new(),
            capture_id: None,
            subject: None,
            predicate: None,
            object: None,
            superseded_by: None,
            decay_rate: 0.0,
            vitality,
            base_weight: 0.0,
            access_count: 0,
            accessed_at: String::new(),
            doc_id: None,
            chunk_index: None,
        }
    }

    fn find<'a>(ranked: &'a [MemorySearchResult], id: &str) -> &'a MemorySearchResult {
        ranked
            .iter()
            .find(|r| r.memory.id == id)
            .unwrap_or_else(|| panic!("{id} missing from ranked results"))
    }

    #[test]
    fn a_memory_present_in_both_lists_outranks_one_present_in_only_one() {
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let semantic = vec![mem("a", 0.0), mem("c", 0.0)];

        let ranked = rank_rrf(keyword, semantic, RrfWeights::default());

        assert_eq!(
            ranked[0].memory.id, "a",
            "top-ranked in both lists beats everything else"
        );
    }

    #[test]
    fn an_empty_semantic_list_means_semantic_never_ran_not_a_tie() {
        // Before #49, `semantic` was always empty because nothing produced
        // vectors -- vec_score must report None here, not a misleading
        // constant that looks like it carries information.
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];

        let ranked = rank_rrf(keyword, vec![], RrfWeights::default());

        assert!(ranked.iter().all(|r| r.vec_score.is_none()));
    }

    #[test]
    fn a_memory_found_by_semantic_search_only_still_gets_a_real_vec_score() {
        // Distinct from the "never ran" case: semantic DID run and ranked
        // this memory, even though keyword search never found it.
        let keyword = vec![mem("a", 0.0)];
        let semantic = vec![mem("b", 0.0)];

        let ranked = rank_rrf(keyword, semantic, RrfWeights::default());

        let b = find(&ranked, "b");
        assert!(b.vec_score.is_some());
        assert!(
            b.fts_score.is_some(),
            "fts_score is always Some, penalty-ranked or not"
        );
    }

    #[test]
    fn a_memory_absent_from_both_lists_is_impossible_the_union_only_contains_seen_ids() {
        let keyword = vec![mem("a", 0.0)];
        let semantic = vec![mem("b", 0.0)];

        let ranked = rank_rrf(keyword, semantic, RrfWeights::default());

        assert_eq!(
            ranked.len(),
            2,
            "the union is exactly [a, b] -- no phantom entries"
        );
    }

    #[test]
    fn a_duplicate_between_both_lists_appears_once_in_the_union() {
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let semantic = vec![mem("b", 0.0), mem("a", 0.0)];

        let ranked = rank_rrf(keyword, semantic, RrfWeights::default());

        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn semantic_favored_weights_can_flip_the_ordering_relative_to_keyword_favored() {
        // "a" ranks #1 by keyword and last by semantic; "b" is the reverse.
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let semantic = vec![mem("b", 0.0), mem("a", 0.0)];

        let keyword_favored = rank_rrf(
            keyword.clone(),
            semantic.clone(),
            RrfWeights {
                w_keyword: 1.5,
                w_semantic: 0.1,
                w_recency: 1.0,
                w_vitality: 0.0,
                w_idf: 0.0,
            },
        );
        let semantic_favored = rank_rrf(
            keyword,
            semantic,
            RrfWeights {
                w_keyword: 0.1,
                w_semantic: 1.5,
                w_recency: 1.0,
                w_vitality: 0.0,
                w_idf: 0.0,
            },
        );

        assert_eq!(keyword_favored[0].memory.id, "a");
        assert_eq!(semantic_favored[0].memory.id, "b");
    }

    #[test]
    fn a_vital_memory_outranks_a_dormant_one_all_else_equal() {
        let keyword = vec![mem("dormant", 0.0), mem("vital", 1.0)];

        let ranked = rank_rrf(keyword, vec![], RrfWeights::default());

        assert_eq!(ranked[0].memory.id, "vital");
    }
}
