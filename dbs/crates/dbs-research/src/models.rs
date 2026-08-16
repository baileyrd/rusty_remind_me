//! Pure data models for the YouTube research pipeline — no I/O, no
//! network. Mirrors `dbs.research.models`.
//!
//! Kept as its own crate (not folded into `dbs-core`) deliberately,
//! same reasoning as the reference: this pipeline has nothing to do
//! with the `Connector`/`Storage`/engine machinery. It's a one-shot,
//! ad-hoc CLI command (`dbs research youtube`), not a backup source.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoMeta {
    pub id: String,
    pub title: String,
    pub url: String,
    pub channel: Option<String>,
    pub subscriber_count: Option<i64>,
    pub view_count: Option<i64>,
    pub duration_seconds: Option<i64>,
    /// yt-dlp `"YYYYMMDD"` string, or `None` if unknown.
    pub upload_date: Option<String>,
}

impl VideoMeta {
    /// `view_count / subscriber_count`. A video with an unknown or
    /// zero subscriber count ranks last rather than raising or being
    /// dropped.
    pub fn engagement(&self) -> f64 {
        match (self.subscriber_count, self.view_count) {
            (Some(subs), Some(views)) if subs > 0 && views > 0 => views as f64 / subs as f64,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub video: VideoMeta,
    pub indexed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AnalysisAnswer {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Default)]
pub struct ResearchResult {
    pub topic: String,
    pub queries: Vec<String>,
    /// Total raw hits across all queries, pre-dedup.
    pub videos_found_raw: usize,
    /// After dedup + recency filter, before truncation to `count`.
    pub videos_deduped: usize,
    /// Final (post-truncation) video set + index result.
    pub outcomes: Vec<IndexOutcome>,
    /// Element 0 is the synthesis/"key findings" answer.
    pub answers: Vec<AnalysisAnswer>,
    pub notebook_name: String,
    pub notebook_id: Option<String>,
    pub infographic_path: Option<String>,
    pub infographic_orientation: Option<String>,
    /// ISO-8601, stamped by `pipeline::run_pipeline`/`run_pipeline_for_videos`.
    pub generated_at: String,
}

impl ResearchResult {
    pub fn indexed_videos(&self) -> Vec<&VideoMeta> {
        self.outcomes
            .iter()
            .filter(|o| o.indexed)
            .map(|o| &o.video)
            .collect()
    }

    pub fn failed_count(&self) -> usize {
        self.outcomes.iter().filter(|o| !o.indexed).count()
    }
}

/// Fatal, non-retryable pipeline failure (no search results at all,
/// or every video failed to index).
#[derive(Debug, Clone)]
pub struct ResearchPipelineError(pub String);

impl fmt::Display for ResearchPipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ResearchPipelineError {}

/// NotebookLM authentication is missing or expired. A `dbs`-owned
/// error type so `dbs-cli` never needs to know anything about the
/// concrete NotebookLM client to catch it.
#[derive(Debug, Clone)]
pub struct NotebookLmAuthError(pub String);

impl fmt::Display for NotebookLmAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for NotebookLmAuthError {}

/// Either half of what can end a pipeline run early.
#[derive(Debug, Clone)]
pub enum ResearchError {
    Pipeline(ResearchPipelineError),
    Auth(NotebookLmAuthError),
}

impl fmt::Display for ResearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pipeline(e) => write!(f, "{e}"),
            Self::Auth(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ResearchError {}

impl ResearchError {
    pub fn pipeline(msg: impl Into<String>) -> Self {
        Self::Pipeline(ResearchPipelineError(msg.into()))
    }

    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(NotebookLmAuthError(msg.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(subs: Option<i64>, views: Option<i64>) -> VideoMeta {
        VideoMeta {
            id: "v1".to_string(),
            title: "title".to_string(),
            url: "https://example.com".to_string(),
            channel: None,
            subscriber_count: subs,
            view_count: views,
            duration_seconds: None,
            upload_date: None,
        }
    }

    #[test]
    fn engagement_divides_views_by_subscribers() {
        assert_eq!(video(Some(100), Some(50)).engagement(), 0.5);
    }

    #[test]
    fn engagement_is_zero_without_both_counts() {
        assert_eq!(video(None, Some(50)).engagement(), 0.0);
        assert_eq!(video(Some(100), None).engagement(), 0.0);
        assert_eq!(video(Some(0), Some(50)).engagement(), 0.0);
    }

    #[test]
    fn indexed_videos_and_failed_count_partition_outcomes() {
        let result = ResearchResult {
            outcomes: vec![
                IndexOutcome {
                    video: video(Some(1), Some(1)),
                    indexed: true,
                    error: None,
                },
                IndexOutcome {
                    video: video(None, None),
                    indexed: false,
                    error: Some("boom".to_string()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(result.indexed_videos().len(), 1);
        assert_eq!(result.failed_count(), 1);
    }
}
