//! YouTube video search for the research pipeline (`dbs research
//! youtube`). Mirrors `dbs.research.youtube_search`.
//!
//! The reference calls yt-dlp's Python library in-process
//! (`extract_info`); per gap-analysis.md's Decision 3, this port shells
//! out to the `yt-dlp` binary instead — `yt-dlp --dump-json
//! --skip-download` for a `ytsearchN:"query"` pseudo-URL, which prints
//! one JSON object per matched video, one per line.
//!
//! Deliberately uses full extraction (the default), not `--flat-playlist`
//! — flat extraction of search results only returns id/title/url;
//! `view_count`, `channel_follower_count` (subscriber count),
//! `duration`, and `upload_date` all come back missing. Those fields are
//! exactly what the recency filter and engagement ranking below need.

use std::process::Command;

use crate::models::{ResearchError, VideoMeta};

fn find_yt_dlp() -> Option<&'static str> {
    Command::new("yt-dlp")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "yt-dlp")
}

/// Searches every query, dedups by video id, applies the recency
/// filter. Ranking/truncation to the final `--count` is a separate,
/// pure step ([`rank_and_truncate`]).
pub fn search_videos(
    queries: &[String],
    per_query: u32,
    months: Option<u32>,
) -> Result<Vec<VideoMeta>, ResearchError> {
    Ok(search_videos_with_stats(queries, per_query, months)?.0)
}

/// Like [`search_videos`], but also returns the raw hit count across
/// all queries before dedup/filtering — used for the pipeline's "N
/// found across M searches, deduplicated to K" reporting.
pub fn search_videos_with_stats(
    queries: &[String],
    per_query: u32,
    months: Option<u32>,
) -> Result<(Vec<VideoMeta>, usize), ResearchError> {
    let Some(yt_dlp) = find_yt_dlp() else {
        return Err(ResearchError::pipeline(
            "the research pipeline needs yt-dlp on PATH; install it and try again.",
        ));
    };
    Ok(search_videos_with_stats_using(
        yt_dlp, queries, per_query, months,
    ))
}

fn search_videos_with_stats_using(
    yt_dlp: &str,
    queries: &[String],
    per_query: u32,
    months: Option<u32>,
) -> (Vec<VideoMeta>, usize) {
    let mut raw = Vec::new();
    for query in queries {
        raw.extend(search_one(yt_dlp, query, per_query));
    }
    let videos: Vec<VideoMeta> = raw.iter().filter_map(entry_to_meta).collect();
    let raw_count = videos.len();
    (dedup_and_filter(videos, months), raw_count)
}

/// Yields raw yt-dlp JSON entries for one query. The only
/// yt-dlp-touching function in this module; a failed/unlaunchable
/// search is a warning (to stderr) and an empty result, not a fatal
/// pipeline error — one bad query out of several shouldn't abort the
/// whole search.
fn search_one(yt_dlp: &str, query: &str, per_query: u32) -> Vec<serde_json::Value> {
    let search_spec = format!("ytsearch{per_query}:\"{query}\"");
    let output = match Command::new(yt_dlp)
        .args([
            "--dump-json",
            "--skip-download",
            "--no-warnings",
            "--ignore-errors",
            &search_spec,
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("research: search {query:?} failed to launch yt-dlp: {e}");
            return Vec::new();
        }
    };
    if !output.status.success() && output.stdout.is_empty() {
        eprintln!(
            "research: search {query:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn entry_to_meta(e: &serde_json::Value) -> Option<VideoMeta> {
    let id = e.get("id")?.as_str()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let url = e
        .get("webpage_url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
    Some(VideoMeta {
        title: e
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)")
            .to_string(),
        url,
        channel: e
            .get("channel")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        subscriber_count: e.get("channel_follower_count").and_then(|v| v.as_i64()),
        view_count: e.get("view_count").and_then(|v| v.as_i64()),
        duration_seconds: e.get("duration").and_then(|v| v.as_i64()),
        upload_date: e
            .get("upload_date")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        id,
    })
}

/// Dedups by id (first-seen-wins across queries); applies the recency
/// filter. A video with an unparseable/missing `upload_date` is KEPT
/// (never silently dropped), flagged via a stderr warning instead —
/// matches this repo's surface-don't-silently-truncate ethos.
fn dedup_and_filter(videos: Vec<VideoMeta>, months: Option<u32>) -> Vec<VideoMeta> {
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<VideoMeta> = videos
        .into_iter()
        .filter(|v| seen.insert(v.id.clone()))
        .collect();

    let Some(months) = months.filter(|m| *m > 0) else {
        return deduped;
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(months) * 30);

    deduped
        .into_iter()
        .filter(|v| match &v.upload_date {
            None => {
                eprintln!(
                    "research: {} ({:?}) has no upload_date; keeping anyway",
                    v.id, v.title
                );
                true
            }
            Some(date) => match chrono::NaiveDate::parse_from_str(date, "%Y%m%d") {
                Ok(d) => d.and_hms_opt(0, 0, 0).unwrap().and_utc() >= cutoff,
                Err(_) => {
                    eprintln!(
                        "research: {} ({:?}) has unparseable upload_date {date:?}; keeping anyway",
                        v.id, v.title
                    );
                    true
                }
            },
        })
        .collect()
}

/// Ranks by engagement (`view_count / subscriber_count`), highest
/// first — a video with an unknown/zero subscriber count ranks last
/// ([`VideoMeta::engagement`] is `0.0` for those). Truncates to
/// `count`.
pub fn rank_and_truncate(mut videos: Vec<VideoMeta>, count: usize) -> Vec<VideoMeta> {
    videos.sort_by(|a, b| b.engagement().total_cmp(&a.engagement()));
    videos.truncate(count);
    videos
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-research-ytsearch-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `yt-dlp` that ignores its args and prints canned NDJSON
    /// (one video per line) to stdout — mirrors the fake-executable-
    /// on-PATH pattern used for `dbs update-ytdlp`'s tests, except
    /// here the fake's absolute path is passed directly rather than
    /// resolved via `PATH`.
    fn fake_yt_dlp(dir: &std::path::Path, entries_ndjson: &str) -> std::path::PathBuf {
        let path = dir.join("fake-yt-dlp.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "cat <<'EOF'\n{entries_ndjson}\nEOF").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn video_json(id: &str, subs: i64, views: i64, upload_date: &str) -> String {
        format!(
            r#"{{"id": "{id}", "title": "Video {id}", "webpage_url": "https://youtu.be/{id}", "channel": "Chan", "channel_follower_count": {subs}, "view_count": {views}, "duration": 120, "upload_date": "{upload_date}"}}"#
        )
    }

    #[test]
    fn search_videos_with_stats_using_parses_ndjson_and_dedups_across_queries() {
        let dir = temp_dir("dedup");
        let today = chrono::Utc::now().format("%Y%m%d").to_string();
        let ndjson = format!(
            "{}\n{}",
            video_json("a", 100, 50, &today),
            video_json("b", 10, 5, &today)
        );
        let script = fake_yt_dlp(&dir, &ndjson);
        let (videos, raw_count) = search_videos_with_stats_using(
            &script.to_string_lossy(),
            &["q1".to_string(), "q2".to_string()],
            10,
            None,
        );
        // Same two videos returned for each of the two queries; dedup
        // collapses to 2, raw_count counts all 4 pre-dedup hits.
        assert_eq!(raw_count, 4);
        assert_eq!(videos.len(), 2);
        assert_eq!(videos[0].channel.as_deref(), Some("Chan"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dedup_and_filter_drops_videos_older_than_the_recency_window() {
        let old = VideoMeta {
            upload_date: Some("20100101".to_string()),
            ..blank_video("old")
        };
        let recent = VideoMeta {
            upload_date: Some(chrono::Utc::now().format("%Y%m%d").to_string()),
            ..blank_video("recent")
        };
        let out = dedup_and_filter(vec![old, recent], Some(6));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "recent");
    }

    #[test]
    fn dedup_and_filter_keeps_videos_with_no_upload_date() {
        let no_date = blank_video("no-date");
        let out = dedup_and_filter(vec![no_date], Some(6));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn dedup_and_filter_with_months_zero_disables_the_recency_filter() {
        let old = VideoMeta {
            upload_date: Some("20100101".to_string()),
            ..blank_video("old")
        };
        let out = dedup_and_filter(vec![old], Some(0));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn rank_and_truncate_ranks_by_engagement_and_truncates() {
        let low = VideoMeta {
            subscriber_count: Some(1000),
            view_count: Some(100),
            ..blank_video("low")
        };
        let high = VideoMeta {
            subscriber_count: Some(10),
            view_count: Some(100),
            ..blank_video("high")
        };
        let ranked = rank_and_truncate(vec![low, high], 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].id, "high");
    }

    fn blank_video(id: &str) -> VideoMeta {
        VideoMeta {
            id: id.to_string(),
            title: id.to_string(),
            url: format!("https://youtu.be/{id}"),
            channel: None,
            subscriber_count: None,
            view_count: None,
            duration_seconds: None,
            upload_date: None,
        }
    }
}
