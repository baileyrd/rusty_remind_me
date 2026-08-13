//! Renders a [`ResearchResult`] as a Markdown research report. Mirrors
//! `dbs.research.report`.
//!
//! Pure function, no I/O. NotebookLM's free-text answers aren't under
//! this codebase's control — each answer is rendered close to verbatim
//! under its own heading rather than parsed/reformatted, so the
//! report's exact structure can vary run to run. That's an accepted
//! trade-off, not a bug to chase.

use crate::models::{IndexOutcome, ResearchResult, VideoMeta};
use crate::pipeline::DEFAULT_QUESTIONS;

const DEFAULT_SECTION_TITLES: [&str; 5] = [
    "Top 5 Highlights",
    "What Worked",
    "Content Gaps",
    "Criticisms",
    "Practical Use Cases",
];

pub fn render_report(result: &ResearchResult) -> String {
    let mut lines: Vec<String> = vec![format!("# Research: {}", result.topic), String::new()];
    lines.push(format!("- **Generated**: {}", result.generated_at));
    let mut notebook_line = format!("- **Notebook**: {}", result.notebook_name);
    if let Some(id) = &result.notebook_id {
        notebook_line.push_str(&format!(" (`{id}`)"));
    }
    lines.push(notebook_line);
    let indexed = result.indexed_videos();
    let mut videos_line = format!(
        "- **Videos analyzed**: {} of {}",
        indexed.len(),
        result.outcomes.len()
    );
    let failed = result.failed_count();
    if failed > 0 {
        videos_line.push_str(&format!(" ({failed} failed to index)"));
    }
    lines.push(videos_line);
    lines.push(String::new());

    if let Some(first) = result.answers.first() {
        lines.push("## Key Findings".to_string());
        lines.push(String::new());
        lines.push(first.answer.trim().to_string());
        lines.push(String::new());
    }

    let non_synthesis = &result.answers[result.answers.len().min(1)..];
    let is_default = non_synthesis.len() == DEFAULT_QUESTIONS.len()
        && non_synthesis
            .iter()
            .zip(DEFAULT_QUESTIONS.iter())
            .all(|(a, q)| a.question == *q);
    for (i, answer) in non_synthesis.iter().enumerate() {
        if is_default && i < DEFAULT_SECTION_TITLES.len() {
            lines.push(format!("## {}", DEFAULT_SECTION_TITLES[i]));
            lines.push(String::new());
        } else {
            lines.push(format!("## Question {}", i + 1));
            lines.push(String::new());
            lines.push(format!("*{}*", answer.question));
            lines.push(String::new());
        }
        lines.push(answer.answer.trim().to_string());
        lines.push(String::new());
    }

    lines.push("## Video Performance & Outliers".to_string());
    lines.push(String::new());
    lines.push("### Top Performers (by views)".to_string());
    lines.push(String::new());
    let mut by_views: Vec<&VideoMeta> = indexed.clone();
    by_views.sort_by_key(|v| std::cmp::Reverse(v.view_count.unwrap_or(0)));
    lines.extend(video_table(&by_views[..by_views.len().min(5)]));
    lines.push(String::new());
    lines.push("### Small Channel Outliers (by engagement)".to_string());
    lines.push(String::new());
    let mut small_channel: Vec<&VideoMeta> = indexed
        .iter()
        .filter(|v| v.subscriber_count.is_some_and(|s| s > 0))
        .copied()
        .collect();
    small_channel.sort_by(|a, b| b.engagement().total_cmp(&a.engagement()));
    lines.extend(video_table(&small_channel[..small_channel.len().min(5)]));
    lines.push(String::new());

    lines.push("## Source Videos".to_string());
    lines.push(String::new());
    lines.extend(source_table(&result.outcomes));
    lines.push(String::new());

    lines.push("## Pipeline Metadata".to_string());
    lines.push(String::new());
    lines.push(format!("- **Queries**: {}", result.queries.join(", ")));
    lines.push(format!(
        "- **Videos found**: {} (across {} search(es), deduplicated to {})",
        result.videos_found_raw,
        result.queries.len(),
        result.videos_deduped
    ));
    lines.push(format!(
        "- **Videos indexed**: {} of {}",
        indexed.len(),
        result.outcomes.len()
    ));
    if failed > 0 {
        lines.push(format!("- **Failed to index**: {failed}"));
    }
    lines.push(format!("- **Questions asked**: {}", result.answers.len()));
    if let Some(path) = &result.infographic_path {
        lines.push(format!(
            "- **Infographic**: {path} ({})",
            result.infographic_orientation.as_deref().unwrap_or("")
        ));
    }
    lines.push(String::new());

    lines.join("\n")
}

fn video_table(videos: &[&VideoMeta]) -> Vec<String> {
    if videos.is_empty() {
        return vec!["_(none)_".to_string()];
    }
    let mut rows = vec![
        "| Title | Channel | Views | Subscribers | Engagement |".to_string(),
        "| --- | --- | --- | --- | --- |".to_string(),
    ];
    for v in videos {
        let views = v
            .view_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let subs = v
            .subscriber_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        rows.push(format!(
            "| [{}]({}) | {} | {} | {} | {:.2} |",
            v.title,
            v.url,
            v.channel.as_deref().unwrap_or("?"),
            views,
            subs,
            v.engagement()
        ));
    }
    rows
}

fn source_table(outcomes: &[IndexOutcome]) -> Vec<String> {
    if outcomes.is_empty() {
        return vec!["_(none)_".to_string()];
    }
    let mut rows = vec![
        "| Title | Channel | Subscribers | Views | Engagement | Duration | Uploaded | Indexed |"
            .to_string(),
        "| --- | --- | --- | --- | --- | --- | --- | --- |".to_string(),
    ];
    for o in outcomes {
        let v = &o.video;
        let duration = v
            .duration_seconds
            .map(|s| format!("{}m", s / 60))
            .unwrap_or_else(|| "?".to_string());
        let subs = v
            .subscriber_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let views = v
            .view_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        rows.push(format!(
            "| [{}]({}) | {} | {} | {} | {:.2} | {} | {} | {} |",
            v.title,
            v.url,
            v.channel.as_deref().unwrap_or("?"),
            subs,
            views,
            v.engagement(),
            duration,
            v.upload_date.as_deref().unwrap_or("?"),
            if o.indexed { "yes" } else { "no" }
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AnalysisAnswer;

    fn video(id: &str, views: Option<i64>, subs: Option<i64>) -> VideoMeta {
        VideoMeta {
            id: id.to_string(),
            title: format!("Video {id}"),
            url: format!("https://youtu.be/{id}"),
            channel: Some("Chan".to_string()),
            subscriber_count: subs,
            view_count: views,
            duration_seconds: Some(125),
            upload_date: Some("20240101".to_string()),
        }
    }

    fn base_result() -> ResearchResult {
        ResearchResult {
            topic: "topic".to_string(),
            queries: vec!["q1".to_string()],
            videos_found_raw: 3,
            videos_deduped: 2,
            outcomes: vec![
                IndexOutcome {
                    video: video("a", Some(100), Some(10)),
                    indexed: true,
                    error: None,
                },
                IndexOutcome {
                    video: video("b", None, None),
                    indexed: false,
                    error: Some("failed".to_string()),
                },
            ],
            answers: vec![AnalysisAnswer {
                question: "synthesis".to_string(),
                answer: "the key findings".to_string(),
            }],
            notebook_name: "Research: topic".to_string(),
            notebook_id: Some("nb1".to_string()),
            infographic_path: None,
            infographic_orientation: None,
            generated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn render_report_includes_the_header_key_findings_and_metadata() {
        let text = render_report(&base_result());
        assert!(text.starts_with("# Research: topic\n"));
        assert!(text.contains("- **Generated**: 2024-01-01T00:00:00Z"));
        assert!(text.contains("- **Notebook**: Research: topic (`nb1`)"));
        assert!(text.contains("- **Videos analyzed**: 1 of 2 (1 failed to index)"));
        assert!(text.contains("## Key Findings"));
        assert!(text.contains("the key findings"));
        assert!(text.contains("## Pipeline Metadata"));
        assert!(text.contains("- **Failed to index**: 1"));
    }

    #[test]
    fn render_report_uses_default_section_titles_for_the_default_questions() {
        let mut result = base_result();
        result
            .answers
            .extend(DEFAULT_QUESTIONS.iter().map(|q| AnalysisAnswer {
                question: q.to_string(),
                answer: "answer text".to_string(),
            }));
        let text = render_report(&result);
        assert!(text.contains("## Top 5 Highlights"));
        assert!(text.contains("## What Worked"));
        assert!(text.contains("## Practical Use Cases"));
        assert!(!text.contains("## Question 1"));
    }

    #[test]
    fn render_report_uses_generic_question_headings_for_custom_questions() {
        let mut result = base_result();
        result.answers.push(AnalysisAnswer {
            question: "a totally custom question".to_string(),
            answer: "an answer".to_string(),
        });
        let text = render_report(&result);
        assert!(text.contains("## Question 1"));
        assert!(text.contains("*a totally custom question*"));
    }

    #[test]
    fn render_report_handles_no_indexed_videos_gracefully() {
        let mut result = base_result();
        result.outcomes = vec![];
        let text = render_report(&result);
        assert!(text.contains("_(none)_"));
    }

    #[test]
    fn video_table_renders_each_video_as_a_row_in_the_given_order() {
        // Ranking happens in `render_report` before calling this — the
        // table itself just formats whatever order it's handed.
        let a = video("a", Some(10), Some(1));
        let b = video("b", Some(100), Some(1));
        let rows = video_table(&[&a, &b]);
        assert_eq!(rows.len(), 4); // header + separator + 2 rows
        assert!(rows[2].contains("Video a"));
        assert!(rows[3].contains("Video b"));
    }
}
