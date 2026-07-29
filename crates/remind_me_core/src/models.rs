use serde::{Deserialize, Serialize};

/// Output format for search operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFormat {
    #[default]
    Markdown,
    Json,
}

/// RRF retrieval strategy profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalStrategy {
    #[default]
    Auto,
    Balanced,
    KeywordFavored,
    SemanticFavored,
}

/// An entity mentioned by a memory (Knowledge Graph).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityInput {
    pub name: String,
    pub kind: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Core Memory data structure stored in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub source: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
    pub capture_id: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub superseded_by: Option<String>,
    pub decay_rate: f64,
    pub vitality: f64,
    pub access_count: i64,
    pub last_accessed_at: String,
}

/// Input model for adding a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryAddInput {
    pub content: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

fn default_category() -> String {
    "general".to_string()
}

fn default_source() -> String {
    "manual".to_string()
}

/// Input model for searching memories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchInput {
    pub query: String,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default)]
    pub response_format: ResponseFormat,
    #[serde(default)]
    pub include_dormant: bool,
    #[serde(default)]
    pub min_vitality: f64,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default)]
    pub expand_entities: bool,
    #[serde(default)]
    pub include_neighbors: bool,
}

fn default_limit() -> usize {
    20
}

fn default_token_budget() -> usize {
    800
}

/// Search result item containing memory and diagnostic ranking scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f64,
    pub fts_score: Option<f64>,
    pub vec_score: Option<f64>,
    pub vitality_score: Option<f64>,
}
