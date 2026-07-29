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
    /// Write-time importance prior. Now a real column, so `effective_vitality`
    /// no longer has to treat the stored `vitality` as a stand-in.
    pub base_weight: f64,
    pub access_count: i64,
    pub accessed_at: String,
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

/// Input model for listing memories with filters and pagination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryListInput {
    pub category: Option<String>,
    /// A memory must carry *all* of these tags to match.
    pub tags: Option<Vec<String>>,
    pub source: Option<String>,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub response_format: ResponseFormat,
}

fn default_list_limit() -> usize {
    20
}

/// Inclusive bounds the reference enforces on `MemoryListInput::limit`.
pub const LIST_LIMIT_MIN: usize = 1;
pub const LIST_LIMIT_MAX: usize = 100;

/// A page of memories plus the total matching the same filters.
///
/// `total` counts every row the filters match, not just the returned page, so
/// callers can paginate without a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryListResult {
    pub memories: Vec<Memory>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

/// Input model for updating a memory. Every field but `memory_id` is optional;
/// omitted fields are left untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUpdateInput {
    pub memory_id: String,
    pub content: Option<String>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
}

/// Outcome of an update attempt.
///
/// `NoFields` is distinct from `Updated` because the reference reports
/// "nothing to update" rather than silently touching `updated_at`.
#[derive(Debug, Clone)]
pub enum UpdateOutcome {
    Updated(Box<Memory>),
    NotFound,
    NoFields,
}

/// Input model for deleting a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDeleteInput {
    pub memory_id: String,
}

/// One memory's annotation: SPO triple fields and entity mentions.
///
/// Every field but `memory_id` is optional; omitted ones are left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryAnnotation {
    pub memory_id: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

/// A batch of annotations to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateInput {
    pub annotations: Vec<MemoryAnnotation>,
}

/// Inclusive bounds the reference enforces on an annotation batch.
pub const ANNOTATE_BATCH_MIN: usize = 1;
pub const ANNOTATE_BATCH_MAX: usize = 100;

/// What happened to one annotation that was applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationApplied {
    pub memory_id: String,
    /// Number of *new* mention links created for this memory.
    pub entities_linked: usize,
}

/// Why one annotation in the batch could not be applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationError {
    pub memory_id: String,
    pub error: String,
}

/// Outcome of an annotation batch.
///
/// Per-item rather than all-or-nothing: one unknown `memory_id` does not
/// discard the rest of the batch, matching the reference, which collects
/// `errors` and continues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotateResult {
    pub results: Vec<AnnotationApplied>,
    pub errors: Vec<AnnotationError>,
}

/// One memory's classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryClassification {
    pub memory_id: String,
    pub memory_type: String,
}

/// A batch of classifications to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyInput {
    pub classifications: Vec<MemoryClassification>,
}

/// Inclusive bounds the reference enforces on a classification batch.
pub const RECLASSIFY_BATCH_MIN: usize = 1;
pub const RECLASSIFY_BATCH_MAX: usize = 100;

/// Outcome of a classification batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyResult {
    pub updated: usize,
    pub not_found: Vec<String>,
    pub total: usize,
}

/// Request for a batch of memories still awaiting classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyBatchInput {
    #[serde(default = "default_reclassify_batch_size")]
    pub batch_size: usize,
}

fn default_reclassify_batch_size() -> usize {
    20
}

/// One unclassified memory, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnclassifiedMemory {
    pub id: String,
    /// First 500 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub tags: Vec<String>,
}

/// A page of memories awaiting classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclassifyBatchResult {
    pub memories: Vec<UnclassifiedMemory>,
    pub total_unclassified: usize,
}

/// The value `memory_type` holds until something classifies it.
pub const UNCLASSIFIED: &str = "unclassified";

/// Input model for `remind_me_feedback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackInput {
    pub memory_id: String,
    pub signal: crate::vitality::FeedbackSignal,
    /// The search query this feedback relates to. Supplying it makes the
    /// feedback contextual rather than a global judgement on the memory.
    pub query: Option<String>,
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

/// Input model for `remind_me_entity_traverse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityTraverseInput {
    /// Entity name **or alias** to start from; casing and spacing are ignored.
    pub name: String,
    /// Traversal depth. Clamped to 1..=3, matching the reference's bounds.
    #[serde(default = "default_traverse_hops")]
    pub hops: u32,
    /// Only follow edges whose relation label matches this exactly.
    #[serde(default)]
    pub relation: Option<String>,
    /// Maximum edges returned across all hops. Clamped to 1..=100.
    #[serde(default = "default_traverse_cap")]
    pub cap: usize,
}

fn default_traverse_hops() -> u32 {
    1
}

fn default_traverse_cap() -> usize {
    crate::entity::RELATION_TRAVERSAL_CAP
}
