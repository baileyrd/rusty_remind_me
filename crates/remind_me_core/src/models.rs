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
    /// Mark this memory sensitive: a "don't surface by default" convenience
    /// flag, **not access control**. This is a single-user store and anyone
    /// with the database reads everything regardless — the flag only keeps a
    /// memory out of ordinary search and list results unless asked for.
    #[serde(default)]
    pub sensitive: bool,
}

fn default_category() -> String {
    "general".to_string()
}

fn default_source() -> String {
    "manual".to_string()
}

/// Input model for searching memories.
///
/// [`Default`] exists for callers that build one programmatically from stored
/// state — a saved search, say — rather than from a tool call, so they can set
/// the handful of fields they care about without restating every expansion
/// flag. It is hand-written rather than derived **on purpose**: a derived
/// `Default` gives `limit: 0` and `token_budget: 0`, which is not a neutral
/// starting point but a search that structurally cannot return anything.
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
    /// Include memories marked sensitive. Off by default, so a sensitive
    /// memory never surfaces in an ordinary search — see
    /// [`MemoryAddInput::sensitive`] for why this is not access control.
    #[serde(default)]
    pub include_sensitive: bool,
    /// Which RRF weight profile to fuse with.
    ///
    /// Defaults to [`RetrievalStrategy::Auto`], which routes on query shape.
    /// The other three pin a preset — an escape hatch, and the thing to reach
    /// for when A/B testing retrieval rather than in ordinary use.
    ///
    /// **Precedence against the environment.** This selects a *multiplier
    /// profile* applied on top of whatever
    /// [`crate::retrieval::RrfWeights::from_env`] resolved, so the
    /// `REMIND_ME_RRF_W_*` variables still set the baseline and a per-call
    /// strategy scales it. `Balanced` applies the identity multiplier, so it
    /// reproduces the configured baseline exactly rather than overriding it
    /// back to the built-in defaults — which is what lets an operator zero a
    /// signal and have it stay zeroed under every strategy.
    #[serde(default)]
    pub strategy: RetrievalStrategy,
    /// Only controls *surfacing*. Associations are reinforced on every search
    /// regardless — see [`crate::expansion::record_co_retrieval`].
    #[serde(default)]
    pub expand_co_retrieval: bool,
}

impl Default for MemorySearchInput {
    /// Every field at the same value `serde` would supply for an absent key,
    /// so a programmatically-built input and a minimal JSON one behave
    /// identically.
    fn default() -> Self {
        Self {
            query: String::new(),
            category: None,
            tags: None,
            limit: default_limit(),
            token_budget: default_token_budget(),
            response_format: ResponseFormat::default(),
            include_dormant: false,
            min_vitality: 0.0,
            verbose: false,
            expand_entities: false,
            include_neighbors: false,
            include_sensitive: false,
            strategy: RetrievalStrategy::default(),
            expand_co_retrieval: false,
        }
    }
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
    /// Include memories marked sensitive. Off by default, so a sensitive
    /// memory never surfaces in an ordinary list — see
    /// [`MemoryAddInput::sensitive`] for why this is not access control.
    #[serde(default)]
    pub include_sensitive: bool,
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
    /// Set or clear the sensitive flag. `None` leaves it alone, which is why
    /// this is an `Option<bool>` rather than a `bool` — without the third
    /// state, every update that did not mention the flag would clear it.
    ///
    /// Not named by issue #105, which lists only add/search/list. Included
    /// because the reference has it (`models.py:382`) and because without it
    /// a memory marked sensitive at creation can never be unmarked.
    #[serde(default)]
    pub sensitive: Option<bool>,
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

/// Input model for `remind_me_consolidate`.
///
/// Two-step workflow, matching the reference's issue #55 update: call with
/// `dry_run: true` (the default) to see the clusters a threshold finds, then
/// write a short `summaries` entry per cluster worth merging and call again
/// with `dry_run: false`. A cluster whose canonical id has no entry in
/// `summaries` is reported in `skipped_no_summary`, not merged with a raw
/// line union.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidateInput {
    /// Minimum cosine similarity to cluster memories together. Clamped to
    /// `CONSOLIDATE_SIMILARITY_MIN..=CONSOLIDATE_SIMILARITY_MAX` rather than
    /// rejected, matching this port's convention elsewhere (e.g.
    /// `EntityTraverseInput::hops`).
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f64,
    /// If true (the default), report clusters without modifying data.
    #[serde(default = "default_consolidate_dry_run")]
    pub dry_run: bool,
    /// Limit consolidation to this category.
    #[serde(default)]
    pub category: Option<String>,
    /// Maximum memories to consider. Clamped to
    /// `CONSOLIDATE_LIMIT_MIN..=CONSOLIDATE_LIMIT_MAX`.
    #[serde(default = "default_consolidate_limit")]
    pub limit: usize,
    /// `{canonical_id: summary}`, one entry per cluster (from a prior
    /// `dry_run: true` call) to actually merge when `dry_run` is false.
    #[serde(default)]
    pub summaries: Option<std::collections::HashMap<String, String>>,
}

fn default_similarity_threshold() -> f64 {
    0.85
}

fn default_consolidate_dry_run() -> bool {
    true
}

fn default_consolidate_limit() -> usize {
    500
}

/// Inclusive bounds the reference enforces on `similarity_threshold`.
pub const CONSOLIDATE_SIMILARITY_MIN: f64 = 0.5;
pub const CONSOLIDATE_SIMILARITY_MAX: f64 = 1.0;
/// Inclusive bounds the reference enforces on `limit`.
pub const CONSOLIDATE_LIMIT_MIN: usize = 10;
pub const CONSOLIDATE_LIMIT_MAX: usize = 5000;

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
///
/// Each `*_score` is that signal's already-weighted contribution to `score`
/// (i.e. `weight / (k + rank)` in RRF-rank fusion, or `weight *
/// normalized_magnitude` in RRF-score fusion) — not the raw rank or
/// magnitude itself. `fts_score` and `idf_score` are always `Some` (the
/// keyword tier always runs); `vec_score` is `None` only when the semantic
/// tier never ran at all (see [`crate::retrieval::rank_rrf`]'s doc comment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f64,
    pub fts_score: Option<f64>,
    pub vec_score: Option<f64>,
    pub recency_score: Option<f64>,
    pub vitality_score: Option<f64>,
    pub idf_score: Option<f64>,
    /// The fractional nudge [`crate::vitality::apply_feedback_adjustment`]
    /// applied to `score` from query-contextual feedback, or `None` when no
    /// stored feedback was similar enough to this query to count.
    pub feedback_adjustment: Option<f64>,
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

/// How to parse an imported file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    /// Detect: `.json`/`.jsonl` are chat; markdown and text are sniffed for
    /// chat role markers.
    #[default]
    Auto,
    Chat,
    /// Per-section or per-paragraph chunking. `.md`/`.markdown`/`.txt` only.
    Document,
}

/// Message extraction strategies a chat import accepts.
pub const EXTRACT_MODES: [&str; 5] = [
    "assistant_messages",
    "user_messages",
    "all_messages",
    "conversations",
    "summaries",
];

/// Inclusive bounds on an import's chunk size.
pub const IMPORT_MAX_LENGTH_MIN: usize = 100;
pub const IMPORT_MAX_LENGTH_MAX: usize = 50_000;

fn default_import_category() -> String {
    "chat_import".to_string()
}

fn default_extract_mode() -> String {
    "assistant_messages".to_string()
}

fn default_import_max_length() -> usize {
    10_000
}

fn default_recursive() -> bool {
    true
}

/// Input model for `remind_me_import_chat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatImportInput {
    pub file_path: String,
    #[serde(default = "default_import_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_extract_mode")]
    pub extract_mode: String,
    #[serde(default = "default_import_max_length")]
    pub max_length: usize,
    #[serde(default)]
    pub kind: ImportKind,
}

/// Input model for `remind_me_import_directory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkImportDirInput {
    pub directory: String,
    #[serde(default = "default_import_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_extract_mode")]
    pub extract_mode: String,
    #[serde(default = "default_import_max_length")]
    pub max_length: usize,
    #[serde(default = "default_recursive")]
    pub recursive: bool,
    #[serde(default)]
    pub kind: ImportKind,
}

/// Inclusive bounds on how many `dbs` items one call pulls.
pub const DBS_IMPORT_LIMIT_MIN: usize = 1;
pub const DBS_IMPORT_LIMIT_MAX: usize = 2000;

fn default_dbs_limit() -> usize {
    500
}

/// Input model for `remind_me_import_dbs`.
///
/// `source` and `item_type` are empty rather than `Option` to match the
/// reference's own model, where empty means "no filter".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbsImportInput {
    /// Path to the `dbs` SQLite archive, inside the allowed import roots.
    pub db_path: String,
    /// Restrict to one `dbs` source name, or empty for all.
    #[serde(default)]
    pub source: String,
    /// Restrict to one `dbs` `item_kind`, or empty for all.
    #[serde(default)]
    pub item_type: String,
    #[serde(default = "default_dbs_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Extra tags added to every imported memory.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Report what would be imported without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Inclusive bounds on how many MemPalace drawers one call pulls.
pub const MEMPALACE_IMPORT_LIMIT_MIN: usize = 1;
pub const MEMPALACE_IMPORT_LIMIT_MAX: usize = 2000;

fn default_mempalace_limit() -> usize {
    500
}

/// Input model for `remind_me_import_mempalace`.
///
/// No path field: unlike [`DbsImportInput`], the store location is operator
/// configuration (`REMIND_ME_MEMPALACE_PATH`), not a per-call argument — see
/// `docs/adr/0001-mempalace-chroma-sqlite-read.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempalaceImportInput {
    /// Restrict to one wing (project), or empty for all.
    #[serde(default)]
    pub wing: String,
    /// Restrict to one room within the wing, or empty for all.
    #[serde(default)]
    pub room: String,
    #[serde(default = "default_mempalace_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Category for a drawer with no restorable frontmatter category.
    #[serde(default)]
    pub category: String,
    /// Extra tags added to every imported memory.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Report what would be imported without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Counts from one import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportStats {
    pub memories_created: usize,
    /// Messages (chat) or chunks (document) before per-message chunking.
    pub raw_entries: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub entities_restored: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub links_restored: usize,
    /// Links whose endpoints were not present, so could not be restored.
    #[serde(skip_serializing_if = "is_zero")]
    pub links_skipped_dangling: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub relations_restored: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub relations_skipped_dangling: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// What happened to one imported file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ImportOutcome {
    Imported {
        import_id: String,
        kind: ImportKind,
        file: String,
        #[serde(flatten)]
        stats: ImportStats,
    },
    /// The same content has been imported before.
    Skipped {
        reason: String,
        file: String,
        import_id: String,
    },
    Failed {
        file: String,
        reason: String,
    },
}

/// Outcome of a directory import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkImportResult {
    pub files_seen: usize,
    pub files_imported: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub memories_created: usize,
    pub results: Vec<ImportOutcome>,
}

// ---------------------------------------------------------------------------
// HTTP API (`remind_me_api`) — bulk operations and paginated search
// ---------------------------------------------------------------------------
//
// These have no MCP-tool equivalent: the reference's bulk routes exist only
// on its dashboard-facing HTTP surface (a dashboard selects a batch from a
// list/search result, then acts on exactly that selection), and paginated
// search with a `total`/`has_more` envelope is likewise an HTTP-only shape —
// the MCP `remind_me_search` tool returns a ranked, token-budgeted list
// instead.

/// A caller-supplied id list, capped for the same reason `ConsolidateInput`
/// caps its candidate pool: a bounded worst case per request. Dashboard
/// selections are user-driven and typically small; this is a safety limit,
/// not an expected size.
pub const BULK_IDS_MAX: usize = 200;

/// Result of a bulk delete: which ids were removed, which named nothing live.
///
/// A missing id does not fail the batch — the rest still applies — so a
/// caller can tell which selections were stale without retrying one at a
/// time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkDeleteResult {
    pub deleted: Vec<String>,
    pub not_found: Vec<String>,
}

/// How [`BulkTagInput`] combines its `tags` with each memory's existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMode {
    /// Union onto the memory's existing tags (the default).
    #[default]
    Add,
    /// Drop these tags if present; anything else is left alone.
    Remove,
    /// Replace the memory's tags wholesale.
    Set,
}

/// Input for a bulk tag operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkTagInput {
    pub ids: Vec<String>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub mode: TagMode,
}

/// Result of a bulk tag operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulkTagResult {
    pub updated: Vec<String>,
    pub not_found: Vec<String>,
}

/// Input to the paginated `GET /api/memories/search` route.
///
/// `query` has already had any `entity:` token stripped by the caller (see
/// [`crate::fts::extract_entity_token`]); `entity` carries that token
/// separately so the entity-scoped path can apply its own rules
/// (superseded exclusion) rather than folding them into the FTS predicate.
#[derive(Debug, Clone)]
pub struct SearchPageInput {
    pub query: String,
    pub category: Option<String>,
    /// A memory must carry *all* of these tags to match.
    pub tags: Option<Vec<String>>,
    pub entity: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

/// A page of [`SearchPageInput`] results, with the same pagination envelope
/// as [`MemoryListResult`] — a client pages through search results the same
/// way it pages through a list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchPageResult {
    pub total: usize,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub memories: Vec<Memory>,
    /// Set when an `entity:` token named an entity this store has never
    /// heard of — the result is a real empty page, not a query error, and
    /// the message says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Importance recalibration (gap T7, issue #102)
// ---------------------------------------------------------------------------

/// Request for a batch of memories whose importance classification may be
/// stale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalibrateCandidatesInput {
    #[serde(default = "default_recalibrate_limit")]
    pub limit: usize,
}

fn default_recalibrate_limit() -> usize {
    20
}

/// Inclusive bounds the reference enforces on a recalibration request.
pub const RECALIBRATE_LIMIT_MIN: usize = 1;
pub const RECALIBRATE_LIMIT_MAX: usize = 100;

/// One memory put forward for importance review.
///
/// Every field is something a reviewer needs to judge from without a second
/// round trip: what it says, how it was classified, how important it was
/// assumed to be, and how long it has gone untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalibrateCandidate {
    pub id: String,
    /// First 500 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub memory_type: Option<String>,
    pub base_weight: f64,
    pub access_count: i64,
    /// `None` for a memory never retrieved since it was written — which is
    /// itself part of why it is a candidate, so it is reported rather than
    /// collapsed into `created_at`.
    pub accessed_at: Option<String>,
    pub created_at: String,
}

/// A page of recalibration candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecalibrateCandidatesResult {
    pub candidates: Vec<RecalibrateCandidate>,
    /// The whole backlog behind the `limit`, so a caller can tell whether
    /// another round is worth requesting.
    pub total_candidates: i64,
}

// ---------------------------------------------------------------------------
// Import rollback (gap T8, issue #103)
// ---------------------------------------------------------------------------

/// Which import ledger an undo targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UndoImportKind {
    Chat,
    Dbs,
    Mempalace,
}

/// Request to roll back a previous import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoImportInput {
    pub import_kind: UndoImportKind,
    /// Scope to one import run. For `chat` this is the `chat_imports`
    /// `import_id`; for `dbs` the `dbs_source`; for `mempalace` a `drawer_id`
    /// prefix. `None` targets every record of that kind.
    #[serde(default)]
    pub import_id: Option<String>,
    /// Defaults to **true**. Bulk deletion that propagates over sync is opt-in,
    /// not opt-out — the asymmetry between an accidental dry run and an
    /// accidental deletion is the whole argument.
    #[serde(default = "default_undo_dry_run")]
    pub dry_run: bool,
    #[serde(default = "default_undo_limit")]
    pub limit: usize,
}

fn default_undo_dry_run() -> bool {
    true
}

fn default_undo_limit() -> usize {
    500
}

/// Inclusive bounds the reference enforces on an undo batch.
pub const UNDO_IMPORT_LIMIT_MIN: usize = 1;
pub const UNDO_IMPORT_LIMIT_MAX: usize = 5000;

/// Outcome of an undo call, dry run or otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoImportResult {
    pub import_kind: UndoImportKind,
    /// Human-readable description of what was targeted, echoed back so a
    /// caller can see whether their scope meant what they thought.
    pub scope: String,
    pub matched: usize,
    pub dry_run: bool,
    /// Whether this was a tombstone or an outright delete, and why.
    pub mode: String,
    pub removed: usize,
    /// What is left after this call — the resumability signal.
    pub remaining: usize,
    pub tracking_rows_removed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

// ---------------------------------------------------------------------------
// Saved and watched searches (gap T3, issue #108)
// ---------------------------------------------------------------------------

/// Result cap a poll uses.
///
/// Higher than a tool call's default: a poll diffs result sets, and a match
/// dropped by a tighter limit would look like it had stopped matching.
pub const POLL_RESULT_LIMIT: usize = 100;

/// The filters stored alongside a saved search's query, as the `filters` JSON
/// column holds them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSearchFilters {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub include_sensitive: bool,
}

/// A stored query plus its filters, under a unique name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSearch {
    pub id: String,
    pub name: String,
    pub query: String,
    pub filters: SavedSearchFilters,
    /// Whether polling reports this search's new matches. Does **not** narrow
    /// what running it returns — see `saved_searches`' module docs.
    pub watch: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Request to create or update a saved search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveSearchInput {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub include_sensitive: bool,
    #[serde(default)]
    pub watch: bool,
}

/// Request naming one saved search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchNameInput {
    pub name: String,
}

// ---------------------------------------------------------------------------
// Edit history (gap T4, issue #109)
// ---------------------------------------------------------------------------

/// One snapshot of a memory's tracked columns, taken before an edit replaced
/// them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRevision {
    pub id: i64,
    pub memory_id: String,
    pub content: String,
    pub category: String,
    /// Stored form (a JSON array string), matching the `memories` column it
    /// snapshots, so a revert can write it straight back.
    pub tags: String,
    pub metadata: String,
    /// `None` for a revision captured before the column existed.
    pub sensitive: Option<bool>,
    pub edited_at: String,
    pub revision_reason: Option<String>,
}

/// Request for a memory's revisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryInput {
    pub memory_id: String,
    #[serde(default = "default_history_limit")]
    pub limit: usize,
}

fn default_history_limit() -> usize {
    20
}

pub const HISTORY_LIMIT_MIN: usize = 1;
pub const HISTORY_LIMIT_MAX: usize = 100;

/// Request to restore a memory to a prior revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertInput {
    pub memory_id: String,
    pub revision_id: i64,
    /// Free text recorded on the revision the revert itself creates.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What a revert did.
///
/// The two not-found cases are distinct because they need different fixes: a
/// missing memory means the id is wrong, a missing revision means the revision
/// id is wrong or belongs to a different memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RevertOutcome {
    Reverted {
        revision_id: i64,
    },
    /// The memory already holds this revision's values.
    NoChange,
    MemoryNotFound,
    RevisionNotFound,
}

// ---------------------------------------------------------------------------
// Contradiction candidates (gap T6, issue #110)
// ---------------------------------------------------------------------------

/// One side of a candidate pair, carrying enough to judge it without a second
/// round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionSide {
    pub id: String,
    /// First 500 characters, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub memory_type: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub created_at: String,
}

/// Two memories that might assert incompatible things.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionCandidate {
    pub memory_a: ContradictionSide,
    pub memory_b: ContradictionSide,
}

/// A page of candidate pairs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionCandidatesResult {
    pub candidates: Vec<ContradictionCandidate>,
    pub total_candidates: i64,
}

/// Request for a batch of contradiction candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionCandidatesInput {
    #[serde(default = "default_contradiction_limit")]
    pub limit: usize,
}

fn default_contradiction_limit() -> usize {
    20
}

pub const CONTRADICTION_LIMIT_MIN: usize = 1;
pub const CONTRADICTION_LIMIT_MAX: usize = 100;
