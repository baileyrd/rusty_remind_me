//! Sync orchestrator for the research commands: videos → NotebookLM
//! synthesis → [`ResearchResult`]. Mirrors `dbs.research.pipeline`.
//!
//! Two entry points share the NotebookLM half:
//!
//! * [`run_pipeline`] — `dbs research youtube`: live YouTube search,
//!   dedup/rank, then synthesize.
//! * [`run_pipeline_for_videos`] — `dbs research youtube-backup`: the
//!   caller already has the videos (pulled from the backup DB);
//!   synthesize only.
//!
//! The reference bridges into its async-only `notebooklm-py` client
//! with a single `asyncio.run()` — this port's [`NotebookLmClient`]
//! trait is plain synchronous method calls instead (a subprocess/MCP
//! call is no more "async" in Rust than any other blocking I/O), so
//! there's no async boundary to bridge here at all.

use crate::models::{AnalysisAnswer, IndexOutcome, ResearchError, ResearchResult, VideoMeta};
use crate::notebooklm::{NotebookLmClient, NotebookLmError};
use crate::youtube_search::{rank_and_truncate, search_videos_with_stats};

pub const SYNTHESIS_QUESTION: &str = "Across all these videos, what are the overall key \
     findings and themes? Summarize concisely.";

pub const DEFAULT_QUESTIONS: [&str; 5] = [
    "What are the top 5 things (ideas, tools, techniques, or claims) discussed most across \
     these videos? Use a numbered heading `### 1. <name>` for each, in order of prominence.",
    "For the videos with the highest views relative to their channel's subscriber count, what \
     specifically seems to have worked (topic angle, format, hook, timing)?",
    "What aspects of this topic do these videos leave uncovered or underexplored?",
    "What criticisms, disagreements, or caveats do these videos raise?",
    "What practical use cases or action items do these videos suggest for someone acting on \
     this topic?",
];

/// Optional knobs shared by [`run_pipeline`] and [`run_pipeline_for_videos`]
/// — mirrors the reference's keyword-only parameters.
#[derive(Default)]
pub struct SynthesisOptions {
    /// Replaces [`DEFAULT_QUESTIONS`] when set.
    pub questions: Option<Vec<String>>,
    pub notebook_name: Option<String>,
    pub infographic: bool,
    /// Only meaningful when `infographic` is set; defaults to `"landscape"`.
    pub infographic_orientation: Option<String>,
    /// Only meaningful when `infographic` is set; defaults to `"infographic.png"`.
    pub infographic_path: Option<String>,
}

/// Searches YouTube for `queries`, feeds the best `count` videos into
/// a fresh NotebookLM notebook, asks the analysis questions, returns
/// the result. `on_progress` receives human-readable status lines —
/// the run takes minutes, so both the CLI (stderr) and a future web UI
/// (SSE) can surface them.
#[allow(clippy::too_many_arguments)]
pub fn run_pipeline(
    topic: &str,
    queries: &[String],
    per_query_count: u32,
    count: usize,
    months: Option<u32>,
    options: SynthesisOptions,
    client: &mut dyn NotebookLmClient,
    mut on_progress: impl FnMut(&str),
) -> Result<ResearchResult, ResearchError> {
    on_progress(&format!(
        "Searching YouTube: {} ({per_query_count} per query)…",
        queries.join(", ")
    ));
    let (deduped, raw_count) = search_videos_with_stats(queries, per_query_count, months)?;
    if deduped.is_empty() {
        return Err(ResearchError::pipeline(format!(
            "no YouTube videos found for {queries:?} (after the recency filter); try a \
             different query or a larger --months window."
        )));
    }
    let deduped_count = deduped.len();
    on_progress(&format!(
        "Found {raw_count} result(s), {deduped_count} after dedup/recency filter."
    ));
    let videos = rank_and_truncate(deduped, count);

    let mut result = synthesize(topic, videos, options, client, &mut on_progress)?;
    result.queries = queries.to_vec();
    result.videos_found_raw = raw_count;
    result.videos_deduped = deduped_count;
    Ok(result)
}

/// Feeds an already-chosen video set (e.g. pulled from the backup DB
/// by `dbs research youtube-backup`) into NotebookLM — no search, no
/// dedup/rank; the caller owns the selection. `source_label` stands in
/// for the search queries in the report's Pipeline Metadata
/// (provenance).
pub fn run_pipeline_for_videos(
    topic: &str,
    videos: Vec<VideoMeta>,
    source_label: &str,
    options: SynthesisOptions,
    client: &mut dyn NotebookLmClient,
    mut on_progress: impl FnMut(&str),
) -> Result<ResearchResult, ResearchError> {
    if videos.is_empty() {
        return Err(ResearchError::pipeline(format!(
            "no videos to research from {source_label}; nothing to send to NotebookLM."
        )));
    }
    let video_count = videos.len();
    let mut result = synthesize(topic, videos, options, client, &mut on_progress)?;
    result.queries = vec![source_label.to_string()];
    result.videos_found_raw = video_count;
    result.videos_deduped = video_count;
    Ok(result)
}

/// The shared NotebookLM half — creates the notebook, indexes every
/// video (per-video failures are tracked, not fatal), asks the
/// synthesis question then every analysis question, optionally
/// generates an infographic. The caller fills in the provenance fields
/// (`queries`/counts) afterward.
fn synthesize(
    topic: &str,
    videos: Vec<VideoMeta>,
    options: SynthesisOptions,
    client: &mut dyn NotebookLmClient,
    on_progress: &mut impl FnMut(&str),
) -> Result<ResearchResult, ResearchError> {
    let questions: Vec<String> = options
        .questions
        .unwrap_or_else(|| DEFAULT_QUESTIONS.iter().map(|q| q.to_string()).collect());
    let notebook_name = options
        .notebook_name
        .unwrap_or_else(|| format!("Research: {topic}"));

    on_progress(&format!("Creating notebook {notebook_name:?}…"));
    let notebook = client.create_notebook(&notebook_name).map_err(wrap)?;

    let mut outcomes = Vec::with_capacity(videos.len());
    let total = videos.len();
    let notebook_id = notebook.id.clone().unwrap_or_default();
    for (i, video) in videos.into_iter().enumerate() {
        on_progress(&format!("[{}/{total}] Indexing: {}", i + 1, video.title));
        match client.add_source(&notebook_id, &video.url) {
            Ok(()) => outcomes.push(IndexOutcome {
                video,
                indexed: true,
                error: None,
            }),
            Err(NotebookLmError::SourceIndex(msg)) => {
                on_progress(&format!(
                    "[{}/{total}] Failed to index ({msg}); continuing.",
                    i + 1
                ));
                outcomes.push(IndexOutcome {
                    video,
                    indexed: false,
                    error: Some(msg),
                });
            }
            Err(e) => return Err(wrap(e)),
        }
    }

    if !outcomes.iter().any(|o| o.indexed) {
        return Err(ResearchError::pipeline(format!(
            "all {} video(s) failed to index into NotebookLM; aborting before asking analysis \
             questions against no real sources.",
            outcomes.len()
        )));
    }

    let total_q = questions.len() + 1;
    on_progress(&format!("[1/{total_q}] Asking synthesis question…"));
    let mut answers = vec![AnalysisAnswer {
        question: SYNTHESIS_QUESTION.to_string(),
        answer: client.ask(&notebook_id, SYNTHESIS_QUESTION).map_err(wrap)?,
    }];
    for (i, question) in questions.into_iter().enumerate() {
        let preview: String = question.chars().take(70).collect();
        on_progress(&format!("[{}/{total_q}] Asking: {preview}…", i + 2));
        let answer = client.ask(&notebook_id, &question).map_err(wrap)?;
        answers.push(AnalysisAnswer { question, answer });
    }

    let mut infographic_path = None;
    let orientation = options
        .infographic_orientation
        .unwrap_or_else(|| "landscape".to_string());
    if options.infographic {
        on_progress("Generating infographic (this can take a few minutes)…");
        let path = options
            .infographic_path
            .unwrap_or_else(|| "infographic.png".to_string());
        infographic_path = Some(
            client
                .generate_infographic(&notebook_id, &path, &orientation)
                .map_err(wrap)?,
        );
    }
    on_progress("Synthesis complete.");

    Ok(ResearchResult {
        topic: topic.to_string(),
        queries: Vec::new(), // provenance filled in by the public entry points
        videos_found_raw: 0, // likewise
        videos_deduped: outcomes.len(),
        outcomes,
        answers,
        notebook_name,
        notebook_id: notebook.id,
        infographic_path,
        infographic_orientation: options.infographic.then_some(orientation),
        generated_at: crate::iso_now(),
    })
}

fn wrap(e: NotebookLmError) -> ResearchError {
    match e {
        NotebookLmError::Auth(msg) => ResearchError::auth(msg),
        NotebookLmError::SourceIndex(msg) | NotebookLmError::Other(msg) => {
            ResearchError::pipeline(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebooklm::Notebook;
    use std::collections::VecDeque;

    /// A scripted [`NotebookLmClient`]: each method pops its next
    /// canned result off a queue, so a test can drive exactly the
    /// sequence of successes/failures it wants to exercise — mirrors
    /// the reference's fake `client_module` swap.
    #[derive(Default)]
    struct FakeClient {
        add_source_results: VecDeque<Result<(), NotebookLmError>>,
        ask_results: VecDeque<Result<String, NotebookLmError>>,
        create_notebook_result: Option<Result<Notebook, NotebookLmError>>,
    }

    impl NotebookLmClient for FakeClient {
        fn create_notebook(&mut self, _title: &str) -> Result<Notebook, NotebookLmError> {
            self.create_notebook_result.take().unwrap_or(Ok(Notebook {
                id: Some("nb1".to_string()),
            }))
        }

        fn add_source(&mut self, _notebook_id: &str, _url: &str) -> Result<(), NotebookLmError> {
            self.add_source_results.pop_front().unwrap_or(Ok(()))
        }

        fn ask(&mut self, _notebook_id: &str, question: &str) -> Result<String, NotebookLmError> {
            self.ask_results
                .pop_front()
                .unwrap_or_else(|| Ok(format!("answer to: {question}")))
        }

        fn generate_infographic(
            &mut self,
            _notebook_id: &str,
            output_path: &str,
            _orientation: &str,
        ) -> Result<String, NotebookLmError> {
            Ok(output_path.to_string())
        }
    }

    fn video(id: &str) -> VideoMeta {
        VideoMeta {
            id: id.to_string(),
            title: format!("Video {id}"),
            url: format!("https://youtu.be/{id}"),
            channel: None,
            subscriber_count: None,
            view_count: None,
            duration_seconds: None,
            upload_date: None,
        }
    }

    #[test]
    fn run_pipeline_for_videos_indexes_asks_and_returns_a_result() {
        let mut client = FakeClient::default();
        let mut lines = Vec::new();
        let result = run_pipeline_for_videos(
            "topic",
            vec![video("a"), video("b")],
            "backup:youtube",
            SynthesisOptions {
                questions: Some(vec!["custom question".to_string()]),
                ..Default::default()
            },
            &mut client,
            |line| lines.push(line.to_string()),
        )
        .unwrap();

        assert_eq!(result.outcomes.len(), 2);
        assert!(result.outcomes.iter().all(|o| o.indexed));
        // synthesis question + 1 custom question
        assert_eq!(result.answers.len(), 2);
        assert_eq!(result.answers[0].question, SYNTHESIS_QUESTION);
        assert_eq!(result.answers[1].question, "custom question");
        assert_eq!(result.notebook_id.as_deref(), Some("nb1"));
        assert_eq!(result.queries, vec!["backup:youtube".to_string()]);
        assert!(lines.iter().any(|l| l.contains("Indexing")));
    }

    #[test]
    fn run_pipeline_for_videos_with_no_videos_is_a_pipeline_error() {
        let mut client = FakeClient::default();
        let err = run_pipeline_for_videos(
            "topic",
            vec![],
            "backup:youtube",
            SynthesisOptions::default(),
            &mut client,
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, ResearchError::Pipeline(_)));
    }

    #[test]
    fn a_per_video_index_failure_is_tracked_not_fatal() {
        let mut client = FakeClient {
            add_source_results: VecDeque::from([
                Err(NotebookLmError::SourceIndex("bad video".to_string())),
                Ok(()),
            ]),
            ..Default::default()
        };
        let result = run_pipeline_for_videos(
            "topic",
            vec![video("a"), video("b")],
            "backup:youtube",
            SynthesisOptions::default(),
            &mut client,
            |_| {},
        )
        .unwrap();
        assert_eq!(result.failed_count(), 1);
        assert_eq!(result.indexed_videos().len(), 1);
        assert_eq!(result.outcomes[0].error.as_deref(), Some("bad video"));
    }

    #[test]
    fn every_video_failing_to_index_aborts_before_asking_questions() {
        let mut client = FakeClient {
            add_source_results: VecDeque::from([
                Err(NotebookLmError::SourceIndex("bad".to_string())),
                Err(NotebookLmError::SourceIndex("bad".to_string())),
            ]),
            ..Default::default()
        };
        let err = run_pipeline_for_videos(
            "topic",
            vec![video("a"), video("b")],
            "backup:youtube",
            SynthesisOptions::default(),
            &mut client,
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, ResearchError::Pipeline(_)));
    }

    #[test]
    fn an_auth_error_from_add_source_aborts_as_a_distinct_error_not_a_per_video_failure() {
        let mut client = FakeClient {
            add_source_results: VecDeque::from([Err(NotebookLmError::Auth(
                "session expired".to_string(),
            ))]),
            ..Default::default()
        };
        let err = run_pipeline_for_videos(
            "topic",
            vec![video("a")],
            "backup:youtube",
            SynthesisOptions::default(),
            &mut client,
            |_| {},
        )
        .unwrap_err();
        match err {
            ResearchError::Auth(e) => assert!(e.0.contains("session expired")),
            other => panic!("expected an auth error, got {other:?}"),
        }
    }

    #[test]
    fn an_auth_error_from_create_notebook_aborts_immediately() {
        let mut client = FakeClient {
            create_notebook_result: Some(Err(NotebookLmError::Auth("no session".to_string()))),
            ..Default::default()
        };
        let err = run_pipeline_for_videos(
            "topic",
            vec![video("a")],
            "backup:youtube",
            SynthesisOptions::default(),
            &mut client,
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, ResearchError::Auth(_)));
    }

    #[test]
    fn generate_infographic_is_only_called_when_requested() {
        let mut client = FakeClient::default();
        let result = run_pipeline_for_videos(
            "topic",
            vec![video("a")],
            "backup:youtube",
            SynthesisOptions {
                infographic: true,
                infographic_path: Some("out.png".to_string()),
                ..Default::default()
            },
            &mut client,
            |_| {},
        )
        .unwrap();
        assert_eq!(result.infographic_path.as_deref(), Some("out.png"));
        assert_eq!(result.infographic_orientation.as_deref(), Some("landscape"));
    }
}
