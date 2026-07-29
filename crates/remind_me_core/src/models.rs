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
    /// Source document this memory was chunked out of, and its position in it.
    /// Both NULL unless an importer produced the row — which is why
    /// `include_neighbors` finds nothing for a manually added memory.
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
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
    /// Surface 1-hop entity-graph neighbours of the results.
    #[serde(default)]
    pub expand_entities: bool,
    /// Surface sibling chunks of the same source document. Only fires for
    /// import-produced memories — anything else has no `doc_id`.
    #[serde(default)]
    pub include_neighbors: bool,
    /// Surface memories frequently retrieved alongside these.
    ///
    /// Only controls *surfacing*. Associations are reinforced on every search
    /// regardless — see [`crate::expansion::record_co_retrieval`].
    #[serde(default)]
    pub expand_co_retrieval: bool,
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

/// Request for a batch of raw imports still awaiting normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeBatchInput {
    #[serde(default = "default_normalize_batch_size")]
    pub batch_size: usize,
}

fn default_normalize_batch_size() -> usize {
    20
}

/// Inclusive bounds the reference enforces on a normalization batch request.
pub const NORMALIZE_BATCH_MIN: usize = 1;
pub const NORMALIZE_BATCH_MAX: usize = 100;

/// Inclusive bounds on an *apply* batch.
///
/// Deliberately half the read bound: the reference asks for up to 100 rows to
/// review and accepts up to 50 distillations back. Not a transcription slip.
pub const NORMALIZE_APPLY_MIN: usize = 1;
pub const NORMALIZE_APPLY_MAX: usize = 50;

/// One raw import awaiting normalization, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnnormalizedMemory {
    pub id: String,
    /// First 1000 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub source: String,
    pub tags: Vec<String>,
    /// Lifted out of `metadata` because it is the one field a reviewer needs to
    /// tell two chunks of an import apart.
    pub filename: Option<String>,
}

/// A page of raw imports awaiting normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeBatchResult {
    pub memories: Vec<UnnormalizedMemory>,
    /// The whole backlog, not just this page, so a caller can tell whether
    /// another round is worth requesting.
    pub total_unnormalized: usize,
}

/// One distillation of a raw import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationEntry {
    pub memory_id: String,
    pub question: String,
    pub summary: String,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    /// Entities the distillation mentions. A raw import is never entity-linked
    /// automatically, so without these the normalized memory is invisible to
    /// `remind_me_entity` and `remind_me_entity_traverse`.
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

/// A batch of distillations to write back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeApplyInput {
    pub normalizations: Vec<NormalizationEntry>,
}

/// One successfully written normalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationOutcome {
    /// The raw memory the distillation came from.
    pub memory_id: String,
    /// The **new** memory holding the distillation.
    pub normalized_id: String,
}

/// One entry that could not be applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizationError {
    pub memory_id: String,
    pub error: String,
}

/// Outcome of an apply batch.
///
/// Unknown ids are reported rather than failing the batch, matching
/// `reclassify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeApplyResult {
    pub normalized: usize,
    pub results: Vec<NormalizationOutcome>,
    pub errors: Vec<NormalizationError>,
}

/// Input model for `remind_me_auto_capture`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoCaptureInput {
    /// The verbatim exchange.
    pub conversation: String,
    /// The distillation of it.
    pub summary: String,
    /// Falls back to the summary's first line, truncated, when empty.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Category for the **summary**. The dialog's category is always
    /// [`DIALOG_CATEGORY`].
    #[serde(default = "default_capture_category")]
    pub category: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

fn default_capture_category() -> String {
    "conversation".to_string()
}

/// Category the verbatim half of a capture is always stored under.
///
/// Not the caller's `category` — that names the summary. `extract_batch`
/// excludes this category, so getting the two the wrong way round would flood
/// the annotation backlog with raw transcripts.
pub const DIALOG_CATEGORY: &str = "dialog";
/// `source` both halves of a capture are stored under.
pub const CAPTURE_SOURCE: &str = "auto_capture";
/// Longest title derived from a summary when none is supplied.
pub const CAPTURE_TITLE_CHARS: usize = 80;

/// The two linked memories a capture produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureResult {
    pub capture_id: String,
    pub dialog_id: String,
    pub summary_id: String,
    pub title: String,
    pub tags: Vec<String>,
    /// The summary's category; the dialog's is always [`DIALOG_CATEGORY`].
    pub category: String,
}

/// A capture retrieved by its `capture_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    pub capture_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog: Option<Memory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Memory>,
    /// Rows sharing the `capture_id` that are neither half — present so a
    /// malformed capture is visible rather than silently dropped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<Memory>,
}

/// Request for a batch of memories still awaiting entity/triple extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractBatchInput {
    #[serde(default = "default_extract_batch_size")]
    pub batch_size: usize,
}

fn default_extract_batch_size() -> usize {
    20
}

/// Inclusive bounds the reference enforces on an extraction batch request.
pub const EXTRACT_BATCH_MIN: usize = 1;
pub const EXTRACT_BATCH_MAX: usize = 100;

/// One memory awaiting extraction, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnannotatedMemory {
    pub id: String,
    /// First 500 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    /// Carried here but not by [`UnclassifiedMemory`] — an extractor benefits
    /// from knowing what a memory has already been classified as.
    pub memory_type: String,
    pub tags: Vec<String>,
}

/// A page of memories awaiting extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractBatchResult {
    pub memories: Vec<UnannotatedMemory>,
    pub total_unannotated: usize,
}

/// One atomic fact extracted from a capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomicFact {
    pub content: String,
    /// Classification, if already known. Defaults to [`UNCLASSIFIED`].
    #[serde(default)]
    pub memory_type: Option<String>,
    /// Merged with the parent capture's tags.
    #[serde(default)]
    pub extra_tags: Vec<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityInput>,
}

/// A batch of facts to write against one capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeInput {
    pub capture_id: String,
    pub facts: Vec<AtomicFact>,
}

/// Inclusive bounds the reference enforces on a decomposition.
pub const DECOMPOSE_FACTS_MIN: usize = 1;
pub const DECOMPOSE_FACTS_MAX: usize = 50;

/// `category` every decomposed fact is stored under.
pub const FACT_CATEGORY: &str = "fact";
/// `source` every decomposed fact is stored under.
pub const DECOMPOSITION_SOURCE: &str = "decomposition";

/// Outcome of a decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeResult {
    pub created: usize,
    pub fact_ids: Vec<String>,
    pub capture_id: String,
    /// The parent capture's tags, which every fact inherited.
    pub parent_tags_inherited: Vec<String>,
    pub entities_linked: usize,
    pub relations_linked: usize,
    /// Memories superseded because a fact contradicted them.
    pub superseded_ids: Vec<String>,
}

/// Request for a batch of captures still awaiting decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeBatchInput {
    #[serde(default = "default_decompose_batch_size")]
    pub batch_size: usize,
}

fn default_decompose_batch_size() -> usize {
    20
}

/// Inclusive bounds on a decomposition batch request.
pub const DECOMPOSE_BATCH_MIN: usize = 1;
pub const DECOMPOSE_BATCH_MAX: usize = 100;

/// One capture awaiting decomposition, trimmed for review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndecomposedCapture {
    pub id: String,
    pub capture_id: String,
    /// First 500 characters of the content.
    pub content_snippet: String,
    pub category: String,
    pub tags: Vec<String>,
}

/// A page of captures awaiting decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposeBatchResult {
    pub memories: Vec<UndecomposedCapture>,
    pub total_undecomposed: usize,
}

/// Serialisation format for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// One indented JSON array.
    #[default]
    Json,
    /// One JSON record per line.
    Jsonl,
}

/// Input model for `remind_me_export_memories`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportInput {
    #[serde(default)]
    pub format: ExportFormat,
    pub category: Option<String>,
    /// A memory must carry *all* of these tags to be exported.
    pub tags: Option<Vec<String>>,
    /// Destination file. Must be inside the allowed export roots. When omitted,
    /// the payload is returned inline.
    pub file_path: Option<String>,
    /// Include the entity graph as `record_type`-tagged records. Defaults to
    /// true — a backup should be complete.
    #[serde(default = "default_include_graph")]
    pub include_graph: bool,
}

fn default_include_graph() -> bool {
    true
}

/// Outcome of an export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    /// Memory records only; the graph counts are reported separately.
    pub exported: usize,
    pub format: ExportFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    /// The payload, when no `file_path` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}
