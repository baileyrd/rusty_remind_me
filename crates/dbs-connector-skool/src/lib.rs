//! Skool connector: backs up your communities/courses/lessons
//! directly (issue #97). Mirrors `dbs.connectors.skool` in
//! baileyrd/Daily-Backup-System.
//!
//! Skool has no public API, but it's a Next.js site: every classroom
//! page embeds a `__NEXT_DATA__` JSON blob describing the community,
//! its courses, and each course's module/lesson tree. The reference
//! loads a captured browser session (Playwright) and reads those
//! blobs straight from the authenticated pages, visiting each
//! lesson's own page to find its video/resources — the course tree
//! itself carries only titles/ids. Native (Mux) video is located by
//! clicking the player and sniffing the browser's resource timeline
//! or a shadow-DOM `<video>.src`, falling back to reconstructing the
//! signed HLS URL from `__NEXT_DATA__`.
//!
//! **Acquisition (issue #188), catalog only.** `fetch()` shells out
//! (via issue #99's `dbs_connector_support::python_launch`) to
//! `scripts/acquire.py`, embedded into the binary via `include_str!`
//! and staged to a temp file at run time — the same split
//! `dbs-connector-reddit` (#187) established. That script's only job
//! is driving a real Chromium page: it navigates wherever Rust tells
//! it to and hands back each page's raw, undecoded `__NEXT_DATA__`
//! blob; every parse below (community/course/lesson extraction,
//! course-selector matching, the raw record → `BackupItem` mapping)
//! is the exact same pure, already-tested Rust code, now reachable
//! from a real run.
//!
//! **Deliberately catalog-only** — no per-lesson page visits, no
//! resource/video downloads, no `.meta.json` sidecar/resume, no
//! GitHub-zip archiving of linked repos. The reference's course tree
//! carries only titles/ids; `videoLink`/`videoId`/`resources` need a
//! visit to each lesson's own page to populate, which this v1 doesn't
//! do — every community backs up the way the reference's
//! `no_download_communities` mode already works (index from the
//! course tree, nothing more). Per-lesson enrichment and the download
//! pipeline are a follow-up, out of scope here (see gap-analysis.md).
//!
//! Everything that's genuinely pure — no browser, no HTTP — is
//! ported and tested here: extracting a community's display name,
//! BFS-searching a `__NEXT_DATA__` tree for a key or a specific
//! lesson node, decoding a lesson's `metadata` fields (which arrive
//! JSON-encoded as strings), matching a configured course selector,
//! reconstructing a Mux HLS URL, matching a URL's host against an
//! allow-list, classifying a permanent-vs-transient video error,
//! parsing memberships/courses/lessons out of a `__NEXT_DATA__` blob,
//! and the community/course/lesson → `BackupItem` mapping. A lesson's
//! `desc` field is rendered to markdown via
//! `dbs_connector_support::tiptap_markdown` (issue #100) for `body`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dbs_connector_support::{find_python, run_python_script_using, tiptap_markdown};
use dbs_core::parse_iso;
use dbs_core::{
    AuthCapture, BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor,
    FetchEvent, ItemKind, MediaRef, ReconcileMarker, RunContext,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SkoolConfig {
    pub downloads_dir: Option<String>,
    /// Empty = auto-discover your joined communities.
    pub communities: Vec<String>,
    /// Title or slug; `"community/course"` scoping.
    pub courses: Vec<String>,
    /// Communities to catalog only (metadata, no downloads).
    pub no_download_communities: Vec<String>,
    pub include_kinds: Vec<String>,
    pub checkpoint_every: u32,
    pub headless: bool,
    pub download_videos: bool,
    /// Cap the downloaded variant's height. 0 = best available.
    pub video_quality: u32,
    /// Env var holding a Netscape cookies.txt for external (non-Mux)
    /// video hosts. `None` disables the check entirely.
    pub video_cookies_file_env: Option<String>,
    pub video_cookies_from_browser: Option<String>,
    pub video_extractor_args: Option<Value>,
    pub video_impersonate_hosts: Vec<String>,
    pub video_debug: bool,
    /// Abandon a video download after this many seconds without
    /// progress. 0 = no watchdog.
    pub video_stall_timeout: u64,
    pub write_markdown: bool,
    pub download_github_repos: bool,
    /// Env var holding the path to the Playwright persistent-context
    /// directory (your logged-in session).
    pub session_dir_env: String,
}

impl Default for SkoolConfig {
    fn default() -> Self {
        Self {
            downloads_dir: None,
            communities: Vec::new(),
            courses: Vec::new(),
            no_download_communities: Vec::new(),
            include_kinds: vec![
                "community".to_string(),
                "course".to_string(),
                "lesson".to_string(),
            ],
            checkpoint_every: 200,
            headless: true,
            download_videos: true,
            video_quality: 1080,
            video_cookies_file_env: Some("YOUTUBE_COOKIES_FILE".to_string()),
            video_cookies_from_browser: None,
            video_extractor_args: None,
            video_impersonate_hosts: vec!["vimeo.com".to_string()],
            video_debug: false,
            video_stall_timeout: 180,
            write_markdown: true,
            download_github_repos: true,
            session_dir_env: "SKOOL_SESSION_DIR".to_string(),
        }
    }
}

pub struct SkoolConnector {
    config: SkoolConfig,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
    pip_requirements: Vec<String>,
    runtime_imports: Vec<String>,
    auth_capture: AuthCapture,
}

impl SkoolConnector {
    pub fn new(config: SkoolConfig) -> Self {
        Self {
            config,
            secret_keys: vec![
                "SKOOL_SESSION_DIR".to_string(),
                "YOUTUBE_COOKIES_FILE".to_string(),
                "GITHUB_TOKEN".to_string(),
            ],
            item_kinds: vec![
                ItemKind {
                    name: "community".to_string(),
                    display_name: "Community".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "course".to_string(),
                    display_name: "Course".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "lesson".to_string(),
                    display_name: "Lesson".to_string(),
                    description: String::new(),
                },
            ],
            volatile_fields: vec!["updatedAt".to_string()],
            pip_requirements: vec![
                "playwright>=1.40".to_string(),
                "yt-dlp[default,curl-cffi]>=2026.1.29".to_string(),
                "nodejs-wheel>=22".to_string(),
                "ffmpeg-downloader>=0.5".to_string(),
            ],
            runtime_imports: vec!["playwright".to_string(), "yt_dlp".to_string()],
            auth_capture: AuthCapture {
                kind: "browser_session".to_string(),
                secret_key: "SKOOL_SESSION_DIR".to_string(),
                login_url: "https://www.skool.com/login".to_string(),
                label: "Skool login".to_string(),
                target_dir_option: String::new(),
                target_path: String::new(),
                per_source: true,
            },
        }
    }

    /// Where files land: an explicit `downloads_dir` wins, else the
    /// engine-provided per-source folder.
    fn downloads_root(&self, ctx: &RunContext) -> Result<PathBuf, ConnectorError> {
        if let Some(d) = &self.config.downloads_dir {
            return Ok(PathBuf::from(d));
        }
        ctx.download_dir.clone().ok_or_else(|| {
            ConnectorError::Config(
                "no download folder: set downloads_dir on the skool source or download_root in \
                 [dbs]."
                    .to_string(),
            )
        })
    }
}

fn value_to_id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A community's display name: `metadata.displayName`, falling back
/// to `metadata.name`, falling back to the group's own `name`.
fn group_name(group: &Value) -> Option<String> {
    let meta = group.get("metadata").cloned().unwrap_or(Value::Null);
    meta.get("displayName")
        .and_then(|v| v.as_str())
        .or_else(|| meta.get("name").and_then(|v| v.as_str()))
        .or_else(|| group.get("name").and_then(|v| v.as_str()))
        .map(str::to_string)
}

/// Breadth-first search over a nested `__NEXT_DATA__`-shaped value
/// for the first object carrying `key`; `None` if never found.
fn deep_find(obj: &Value, key: &str) -> Option<Value> {
    let mut queue: VecDeque<Value> = VecDeque::new();
    queue.push_back(obj.clone());
    while let Some(cur) = queue.pop_front() {
        match cur {
            Value::Object(map) => {
                if let Some(v) = map.get(key) {
                    return Some(v.clone());
                }
                for v in map.into_values() {
                    queue.push_back(v);
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    queue.push_back(v);
                }
            }
            _ => {}
        }
    }
    None
}

/// Skool's `metadata` map is string-valued; structured fields (video,
/// resources, ...) arrive JSON-encoded. Parses a JSON-encoded string
/// value; passes anything else through unchanged; a string that
/// isn't valid JSON is returned as-is (it's just a plain string
/// field, not an encoding failure).
fn json_field(value: &Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| value.clone()),
        other => other.clone(),
    }
}

/// Decodes one lesson node's video/resources/description out of its
/// (possibly JSON-encoded) `metadata`, normalizing resources that
/// carry a bare `link` (no `downloadUrl`) into external references.
fn lesson_fields(node: &Value) -> Value {
    let meta = node.get("metadata").cloned().unwrap_or(Value::Null);
    let video_raw = meta
        .get("video")
        .filter(|v| !v.is_null())
        .cloned()
        .or_else(|| node.get("video").cloned())
        .unwrap_or(Value::Null);
    let video = json_field(&video_raw);
    let video_url = match &video {
        Value::Object(_) => video
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        Value::String(s) if s.starts_with("http") => Some(s.clone()),
        _ => None,
    };

    let mut raw_resources = meta.get("resources").cloned();
    let is_absent = raw_resources
        .as_ref()
        .map(|v| v.is_null() || v.as_str() == Some(""))
        .unwrap_or(true);
    if is_absent {
        raw_resources = node.get("resources").cloned();
    }
    let resources_val = raw_resources.map(|v| json_field(&v)).unwrap_or(Value::Null);
    let resources_arr: Vec<Value> = match resources_val {
        Value::Object(_) => vec![resources_val],
        Value::Array(a) => a,
        _ => Vec::new(),
    };
    let normalized: Vec<Value> = resources_arr
        .into_iter()
        .filter_map(|r| {
            let Value::Object(mut obj) = r else {
                return None;
            };
            let link = obj
                .get("link")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let has_download_url = obj.get("downloadUrl").is_some_and(|v| !v.is_null());
            if let Some(l) = link {
                if !has_download_url {
                    obj.insert("downloadUrl".to_string(), Value::String(l));
                    obj.insert("isExternal".to_string(), Value::Bool(true));
                }
            }
            Some(Value::Object(obj))
        })
        .collect();

    let video_link = meta
        .get("videoLink")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(video_url);
    serde_json::json!({
        "videoLink": video_link,
        "videoId": meta.get("videoId").cloned().unwrap_or(Value::Null),
        "resources": normalized,
        "desc": meta.get("desc").cloned().unwrap_or(Value::Null),
    })
}

/// BFS-locates the lesson node matching `lesson_id` within a
/// classroom page's `__NEXT_DATA__`: first under `props.course`
/// (requiring an object `metadata`), then two named fallbacks
/// (`props.lesson`, `props.course.course`, neither id-checked), then
/// finally the whole document.
///
/// Unreachable from `fetch()` in this catalog-only v1 (#188) — it
/// exists for a future per-lesson-page-visit enrichment step, which
/// is the only caller that would ever need to locate a single lesson
/// node within a *lesson page's* `__NEXT_DATA__`. Kept alive by tests.
#[allow(dead_code)]
fn find_lesson_node(next_data: &Value, lesson_id: &str) -> Option<Value> {
    if lesson_id.is_empty() {
        return None;
    }
    fn bfs(root: &Value, lesson_id: &str) -> Option<Value> {
        let mut queue: VecDeque<Value> = VecDeque::new();
        queue.push_back(root.clone());
        while let Some(cur) = queue.pop_front() {
            match &cur {
                Value::Object(map) => {
                    let id_matches = map
                        .get("id")
                        .and_then(value_to_id_string)
                        .is_some_and(|id| id == lesson_id);
                    let has_metadata_obj = map.get("metadata").is_some_and(|v| v.is_object());
                    if id_matches && has_metadata_obj {
                        return Some(cur);
                    }
                    for v in map.values() {
                        queue.push_back(v.clone());
                    }
                }
                Value::Array(arr) => {
                    for v in arr {
                        queue.push_back(v.clone());
                    }
                }
                _ => {}
            }
        }
        None
    }
    let props = next_data
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .cloned()
        .unwrap_or(Value::Null);
    let course_val = props.get("course").cloned().unwrap_or(Value::Null);
    if let Some(found) = bfs(&course_val, lesson_id) {
        return Some(found);
    }
    let fallbacks = [
        props.get("lesson").cloned(),
        props.get("course").and_then(|c| c.get("course")).cloned(),
    ];
    for fallback in fallbacks.into_iter().flatten() {
        if fallback.is_object() && fallback.get("id").is_some_and(|v| !v.is_null()) {
            return Some(fallback);
        }
    }
    bfs(next_data, lesson_id)
}

/// Whether `course` matches any of the configured `selectors`
/// (`communities`/`courses` config lists), by title or slug,
/// case-insensitively, with an optional `"community/course"` scope
/// prefix. No selectors means everything matches.
fn course_selected(selectors: &[String], community_slug: &str, course: &Value) -> bool {
    if selectors.is_empty() {
        return true;
    }
    let title = course
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let slug = course
        .get("slug")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    for sel in selectors {
        let mut want = sel.trim().to_lowercase();
        if let Some(idx) = want.find('/') {
            let comm = want[..idx].trim().to_string();
            let rest = want[idx + 1..].trim().to_string();
            if comm != community_slug.trim().to_lowercase() {
                continue;
            }
            want = rest;
        }
        if !want.is_empty() && (want == title || want == slug) {
            return true;
        }
    }
    false
}

/// Reconstructs the signed Mux HLS URL from a classroom page's
/// `__NEXT_DATA__`, when its embedded video id matches `video_id`
/// exactly.
///
/// Unreachable from `fetch()` in this catalog-only v1 (#188) — native
/// video URL resolution needs the per-lesson video-download pipeline,
/// out of scope here. Kept alive by tests.
#[allow(dead_code)]
fn mux_hls_url(next_data: &Value, video_id: &Value) -> Option<String> {
    if video_id.is_null() {
        return None;
    }
    let props = next_data
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .cloned()
        .unwrap_or(Value::Null);
    let video = props
        .get("video")
        .cloned()
        .or_else(|| props.get("course").and_then(|c| c.get("video")).cloned())?;
    if !video.is_object() {
        return None;
    }
    if video.get("id") != Some(video_id) {
        return None;
    }
    let pid = video.get("playbackId").and_then(|v| v.as_str())?;
    let token = video.get("playbackToken").and_then(|v| v.as_str())?;
    Some(format!(
        "https://stream.video.skool.com/{pid}.m3u8?token={token}"
    ))
}

/// Whether `url`'s host is `hosts[i]` or a subdomain of it (`www.`
/// stripped from the URL's own host first).
///
/// Unreachable from `fetch()` in this catalog-only v1 (#188) — used to
/// decide which external video hosts need TLS-fingerprint
/// impersonation, part of the not-yet-ported download pipeline. Kept
/// alive by tests.
#[allow(dead_code)]
fn url_host_matches(url_str: &str, hosts: &[String]) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let mut host = parsed.host_str().unwrap_or("").to_lowercase();
    if let Some(stripped) = host.strip_prefix("www.") {
        host = stripped.to_string();
    }
    hosts.iter().any(|h| {
        let h = h.trim().trim_start_matches('.').to_lowercase();
        !h.is_empty() && (host == h || host.ends_with(&format!(".{h}")))
    })
}

/// `"unavailable"` for errors that will never resolve on retry
/// (removed/private/terminated/DMCA'd), `"failed"` for everything
/// else (transient, retried on a later run). Deliberately does not
/// match a bot-check message — that stays retryable.
///
/// Unreachable from `fetch()` in this catalog-only v1 (#188) — part of
/// the not-yet-ported video-download pipeline. Kept alive by tests.
#[allow(dead_code)]
fn classify_video_error(message: &str) -> &'static str {
    const PERMANENT_PATTERNS: &[&str] = &[
        "video unavailable",
        "has been removed",
        "removed by the uploader",
        "this video is private",
        "private video",
        "no longer available",
        "account terminated",
        "account has been terminated",
        "account associated with this video has been terminated",
        "violating",
        "violation",
        "removed for violating",
    ];
    let lower = message.to_lowercase();
    if PERMANENT_PATTERNS.iter().any(|p| lower.contains(p)) {
        "unavailable"
    } else {
        "failed"
    }
}

/// Parses the logged-in account's joined communities out of a
/// `__NEXT_DATA__` blob (from `self.allGroups`), deduplicated by
/// slug.
///
/// Unreachable from `fetch()` in this catalog-only v1 (#188) —
/// auto-discovery needs only the bare slug list to know which
/// classroom pages to visit next, so `scripts/acquire.py` does that
/// minimal extraction itself (see its module doc-comment); this
/// richer, tested parse (id/displayName included) is kept for when a
/// caller needs more than slugs. Kept alive by tests.
#[allow(dead_code)]
fn parse_memberships(next_data: &Value) -> Vec<Value> {
    let props = next_data
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .cloned()
        .unwrap_or(Value::Null);
    let self_val = props
        .get("self")
        .filter(|v| v.is_object())
        .cloned()
        .or_else(|| deep_find(next_data, "self"))
        .unwrap_or(Value::Null);
    let all_groups = self_val
        .get("allGroups")
        .filter(|v| v.is_array())
        .cloned()
        .or_else(|| deep_find(next_data, "allGroups"))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for m in all_groups {
        if !m.is_object() {
            continue;
        }
        let inner = m
            .get("group")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(Value::Null);
        let Some(slug) = m
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| inner.get("name").and_then(|v| v.as_str()))
        else {
            continue;
        };
        if !seen.insert(slug.to_string()) {
            continue;
        }
        let meta = m
            .get("metadata")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| inner.get("metadata").cloned())
            .unwrap_or(Value::Null);
        let display_name = meta
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or(slug);
        out.push(serde_json::json!({
            "slug": slug,
            "id": m.get("id").cloned().or_else(|| inner.get("id").cloned()).unwrap_or(Value::Null),
            "displayName": display_name,
        }));
    }
    out
}

/// Parses the community's course catalog out of a classroom page's
/// `__NEXT_DATA__`.
fn parse_courses(next_data: &Value) -> Vec<Value> {
    let props = next_data
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .cloned()
        .unwrap_or(Value::Null);
    let raw = props
        .get("allCourses")
        .filter(|v| v.is_array())
        .cloned()
        .or_else(|| {
            props
                .get("renderData")
                .and_then(|r| r.get("allCourses"))
                .filter(|v| v.is_array())
                .cloned()
        })
        .or_else(|| deep_find(next_data, "allCourses"))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    raw.into_iter()
        .filter(|c| c.is_object())
        .map(|c| {
            let meta = c.get("metadata").cloned().unwrap_or(Value::Null);
            let has_access = match meta.get("hasAccess").and_then(|v| v.as_i64()) {
                Some(1) => Value::Bool(true),
                Some(0) => Value::Bool(false),
                _ => Value::Null,
            };
            let id = c.get("id").cloned().unwrap_or(Value::Null);
            let name = c.get("name").and_then(|v| v.as_str());
            let slug = name.map(str::to_string).or_else(|| value_to_id_string(&id));
            let title = meta
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| name.map(str::to_string))
                .or_else(|| value_to_id_string(&id));
            serde_json::json!({
                "id": id,
                "slug": slug,
                "title": title,
                "coverImageUrl": meta.get("coverImage").and_then(|v| v.as_str())
                    .or_else(|| meta.get("coverSmallUrl").and_then(|v| v.as_str()))
                    .or_else(|| meta.get("image").and_then(|v| v.as_str())),
                "updatedAt": meta.get("updatedAt").cloned().or_else(|| c.get("updatedAt").cloned()).unwrap_or(Value::Null),
                "hasAccess": has_access,
                "privacy": meta.get("privacy").cloned().unwrap_or(Value::Null),
                "numModules": meta.get("numModules").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Parses a course page's `__NEXT_DATA__` module/lesson tree into a
/// flat list of raw lesson records — a wrapper node with children is
/// a module (each child becomes a lesson tagged with the module's
/// title), a childless wrapper is a bare lesson.
fn parse_lessons(course_next_data: &Value) -> Vec<Value> {
    fn unwrap_node(node: &Value) -> Value {
        node.get("course")
            .filter(|c| c.is_object())
            .cloned()
            .unwrap_or_else(|| node.clone())
    }
    fn emit(out: &mut Vec<Value>, payload: &Value, module_title: Option<&str>) {
        let meta = payload.get("metadata").cloned().unwrap_or(Value::Null);
        let fields = lesson_fields(payload);
        let title = meta
            .get("title")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("name").and_then(|v| v.as_str()));
        let updated_at = meta
            .get("updatedAt")
            .cloned()
            .or_else(|| payload.get("updatedAt").cloned())
            .unwrap_or(Value::Null);
        let has_video = fields.get("videoLink").is_some_and(|v| !v.is_null())
            || fields.get("videoId").is_some_and(|v| !v.is_null());
        let mut rec = serde_json::json!({
            "lessonId": payload.get("id").cloned().unwrap_or(Value::Null),
            "title": title,
            "moduleTitle": module_title,
            "updatedAt": updated_at,
            "hasVideo": has_video,
        });
        if let (Value::Object(map), Value::Object(fmap)) = (&mut rec, fields) {
            for (k, v) in fmap {
                map.insert(k, v);
            }
        }
        out.push(rec);
    }

    let props = course_next_data
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .cloned()
        .unwrap_or(Value::Null);
    let course = props.get("course").cloned().unwrap_or(Value::Null);
    let mut out = Vec::new();
    let Some(children) = course.get("children").and_then(|v| v.as_array()) else {
        return out;
    };
    for node in children {
        if !node.is_object() {
            continue;
        }
        let child_children = node
            .get("children")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if !child_children.is_empty() {
            let payload = unwrap_node(node);
            let module_title = payload
                .get("metadata")
                .and_then(|m| m.get("title"))
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("name").and_then(|v| v.as_str()))
                .map(str::to_string);
            for child in &child_children {
                if !child.is_object() {
                    continue;
                }
                let child_payload = unwrap_node(child);
                emit(&mut out, &child_payload, module_title.as_deref());
            }
        } else {
            let payload = unwrap_node(node);
            emit(&mut out, &payload, None);
        }
    }
    out
}

/// Extracts the community slug from a `communities` config entry —
/// either a bare slug or a full `skool.com/<slug>` URL.
fn slug_from_community(community: &str) -> String {
    if let Some(idx) = community.find("skool.com/") {
        let rest = &community[idx + "skool.com/".len()..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    community.trim_matches(|c| c == '/' || c == ' ').to_string()
}

fn community_item(raw: &Value) -> Option<BackupItem> {
    let slug = raw
        .get("slug")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("groupName").and_then(|v| v.as_str()))?;
    let mut item = BackupItem::new(format!("community:{slug}"), "community", raw.clone()).ok()?;
    item.title = raw
        .get("groupName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| Some(slug.to_string()));
    item.updated_at = raw
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    Some(item)
}

fn course_item(raw: &Value) -> Option<BackupItem> {
    let name = raw.get("courseName").and_then(|v| v.as_str())?;
    let group = raw
        .get("_group_slug")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("groupName").and_then(|v| v.as_str()))
        .unwrap_or("");
    let mut media = Vec::new();
    if let Some(cover) = raw
        .get("courseImageUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        media.push(MediaRef {
            url: cover.to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        });
    }
    let tags: Vec<String> = raw
        .get("groupName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
    let ext_id = if group.is_empty() {
        format!("course:{name}")
    } else {
        format!("course:{group}/{name}")
    };
    let mut item = BackupItem::new(ext_id, "course", raw.clone()).ok()?;
    item.title = Some(name.to_string());
    item.tags = tags;
    item.media = media;
    item.updated_at = raw
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    Some(item)
}

fn lesson_item(raw: &Value) -> Option<BackupItem> {
    let lesson_id = value_to_id_string(raw.get("lessonId")?)?;
    let mut media = Vec::new();
    if let Some(resources) = raw.get("_resources").and_then(|v| v.as_array()) {
        for res in resources {
            let path = res
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mime = res.get("mime").and_then(|v| v.as_str()).map(str::to_string);
            let kind = if mime.as_deref().unwrap_or("").starts_with("image/") {
                "image"
            } else {
                "file"
            };
            media.push(MediaRef {
                url: path,
                kind: kind.to_string(),
                filename: res
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                mime,
                data: None,
            });
        }
    }
    let video_path = raw.get("_video_path").and_then(|v| v.as_str());
    let video_link = raw
        .get("videoLink")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let video_unavailable = raw
        .get("videoUnavailable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(vp) = video_path {
        media.push(MediaRef {
            url: vp.to_string(),
            kind: "video".to_string(),
            filename: std::path::Path::new(vp)
                .file_name()
                .map(|n| n.to_string_lossy().to_string()),
            mime: None,
            data: None,
        });
    } else if let Some(vl) = video_link {
        if !video_unavailable {
            media.push(MediaRef {
                url: vl.to_string(),
                kind: "video".to_string(),
                filename: None,
                mime: None,
                data: None,
            });
        }
    }
    let tags: Vec<String> = [
        raw.get("_group_name").and_then(|v| v.as_str()),
        raw.get("_course_name").and_then(|v| v.as_str()),
        raw.get("moduleTitle").and_then(|v| v.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .map(str::to_string)
    .collect();
    let body = raw
        .get("desc")
        .map(tiptap_markdown)
        .filter(|s| !s.is_empty());
    let mut item = BackupItem::new(lesson_id, "lesson", raw.clone()).ok()?;
    item.title = raw
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.body = body;
    item.tags = tags;
    item.media = media;
    item.updated_at = raw
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    Some(item)
}

/// Dispatches a raw walk record (tagged `_kind`) to its mapper.
fn to_item(raw: &Value) -> Option<BackupItem> {
    match raw.get("_kind").and_then(|v| v.as_str()) {
        Some("community") => community_item(raw),
        Some("course") => course_item(raw),
        Some("lesson") => lesson_item(raw),
        _ => None,
    }
}

// -- acquisition (Playwright-driven, via a Python subprocess; #188) -----

/// The embedded acquisition script — staged to a temp file at run time
/// and run through `dbs_connector_support::python_launch`. See the
/// module doc-comment for the two-call (`communities`/`courses`) design.
const ACQUIRE_SCRIPT: &str = include_str!("../scripts/acquire.py");

/// How long a single acquisition call (browser launch + however many
/// page navigations it was given) may take before being abandoned.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(900);

fn script_error_to_connector_error(kind: &str, message: String) -> ConnectorError {
    match kind {
        "auth" => ConnectorError::Auth(message),
        "rate_limited" => ConnectorError::RateLimited(message),
        "config" => ConnectorError::Config(message),
        _ => ConnectorError::Transient(message),
    }
}

/// Runs the acquisition script in `mode` (`"communities"` or
/// `"courses"`) under `interpreter` and returns the parsed JSON result
/// object on success. Split from [`acquire`] so tests can inject a
/// fake interpreter/script instead of real Python + Playwright + a
/// live Skool session.
fn acquire_using(
    interpreter: &str,
    script: &Path,
    mode: &str,
    session_dir: &str,
    headless: bool,
    payload: &Value,
    timeout: Duration,
) -> Result<Value, ConnectorError> {
    let args = vec![
        mode.to_string(),
        session_dir.to_string(),
        headless.to_string(),
        payload.to_string(),
    ];
    let output = run_python_script_using(interpreter, script, &args, timeout).map_err(|e| {
        ConnectorError::Transient(format!(
            "skool: failed to run the {mode} acquisition script: {e}"
        ))
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout.lines().last().unwrap_or("").trim();
    if last_line.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ConnectorError::Transient(format!(
            "skool: {mode} acquisition script produced no output (exit {:?}); stderr: {}",
            output.status.code(),
            stderr.trim()
        )));
    }
    let parsed: Value = serde_json::from_str(last_line).map_err(|e| {
        ConnectorError::Transient(format!(
            "skool: {mode} acquisition script produced unparseable output ({e}): {last_line}"
        ))
    })?;
    if !parsed.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let kind = parsed
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("transient");
        let message = parsed
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("skool: acquisition failed")
            .to_string();
        return Err(script_error_to_connector_error(kind, message));
    }
    Ok(parsed)
}

/// Stages [`ACQUIRE_SCRIPT`] to a temp file and runs it through
/// whichever interpreter [`find_python`] resolves.
fn acquire(
    mode: &str,
    session_dir: &str,
    headless: bool,
    payload: &Value,
) -> Result<Value, ConnectorError> {
    let interpreter = find_python().ok_or_else(|| {
        ConnectorError::Config(
            "the Skool connector needs Playwright; install it with `pip install playwright` \
             and run `playwright install chromium` (no python3/python interpreter found on \
             PATH)."
                .to_string(),
        )
    })?;
    let script_path = std::env::temp_dir().join(format!(
        "dbs-connector-skool-acquire-{}-{:?}.py",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&script_path, ACQUIRE_SCRIPT).map_err(|e| {
        ConnectorError::Transient(format!(
            "skool: failed to stage the acquisition script: {e}"
        ))
    })?;
    let result = acquire_using(
        interpreter,
        &script_path,
        mode,
        session_dir,
        headless,
        payload,
        ACQUIRE_TIMEOUT,
    );
    let _ = std::fs::remove_file(&script_path);
    result
}

impl Connector for SkoolConnector {
    fn type_name(&self) -> &str {
        "skool"
    }

    fn display_name(&self) -> &str {
        "Skool (courses)"
    }

    fn description(&self) -> &str {
        "Your Skool communities, courses, and lessons via a logged-in browser session."
    }

    fn docs_url(&self) -> &str {
        "https://github.com/baileyrd/skool-downloader"
    }

    fn setup_hint(&self) -> &str {
        "Click 'Skool login' to capture a session: a browser opens, you log in, and you CLOSE \
         the window to finish. Resource files are saved under <download_root>/<source-name> \
         unless downloads_dir overrides it."
    }

    fn secret_keys(&self) -> &[String] {
        &self.secret_keys
    }

    fn wants_managed_http(&self) -> bool {
        false
    }

    fn volatile_fields(&self) -> &[String] {
        &self.volatile_fields
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn pip_requirements(&self) -> &[String] {
        &self.pip_requirements
    }

    fn runtime_imports(&self) -> &[String] {
        &self.runtime_imports
    }

    fn needs_playwright_browser(&self) -> bool {
        true
    }

    fn auth_capture(&self) -> Option<&AuthCapture> {
        Some(&self.auth_capture)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: false,
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: true,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: false,
            paginated: false,
            concurrency: "serial".to_string(),
            ..Capabilities::default()
        }
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        if let Some(env) = &self.config.video_cookies_file_env {
            if !env.is_empty() && !self.secret_keys.contains(env) {
                out.push(Err(ConnectorError::Config(format!(
                    "video_cookies_file_env={env:?} must be one of the declared secret_keys \
                     {:?}; set it in your .env, or set video_cookies_from_browser in the source \
                     config instead.",
                    self.secret_keys
                ))));
                return Box::new(out.into_iter());
            }
        }

        let session_dir = match ctx.secrets.get(&self.config.session_dir_env) {
            Ok(p) => p.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        if !std::path::Path::new(&session_dir).exists() {
            out.push(Err(ConnectorError::Config(format!(
                "Skool session directory {session_dir} does not exist; capture a login once \
                 (the web UI's 'Skool login' button) to create it."
            ))));
            return Box::new(out.into_iter());
        }

        if let Err(e) = self.downloads_root(ctx) {
            out.push(Err(e));
            return Box::new(out.into_iter());
        }

        // -- phase 1: communities (auto-discovered or explicit) -----
        let auto_discover = self.config.communities.is_empty();
        let communities_payload = if auto_discover {
            serde_json::json!([])
        } else {
            serde_json::json!(self
                .config
                .communities
                .iter()
                .map(|c| slug_from_community(c))
                .collect::<Vec<_>>())
        };
        let communities_result = match acquire(
            "communities",
            &session_dir,
            self.config.headless,
            &communities_payload,
        ) {
            Ok(r) => r,
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        let communities: Vec<Value> = communities_result
            .get("communities")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if auto_discover && communities.is_empty() {
            out.push(Err(ConnectorError::Auth(
                "skool: could not auto-detect any joined communities — the captured session may \
                 be degraded (still loads pages, but reports no memberships). If the session is \
                 actually fine and the account has joined communities, set `communities` \
                 explicitly instead of relying on auto-detection."
                    .to_string(),
            )));
            return Box::new(out.into_iter());
        }
        if communities.is_empty() {
            eprintln!(
                "skool: no communities to back up — set `communities` in the source config, or \
                 join a community with the logged-in account."
            );
        }

        let mut live_by_group: HashMap<String, HashSet<String>> = HashMap::new();
        let mut community_complete: HashMap<String, bool> = HashMap::new();
        let mut skipped_courses: HashMap<String, u32> = HashMap::new();
        // (community slug, course slug) -> (group display name, parsed course record)
        let mut course_lookup: HashMap<(String, String), (String, Value)> = HashMap::new();
        let mut course_pairs: Vec<(String, String)> = Vec::new();
        let mut seen: u32 = 0;

        for entry in &communities {
            let Some(slug) = entry.get("slug").and_then(Value::as_str) else {
                continue;
            };
            let slug = slug.to_string();
            let next_data = entry.get("next_data").cloned().unwrap_or(Value::Null);
            let props = next_data
                .get("props")
                .and_then(|p| p.get("pageProps"))
                .cloned()
                .unwrap_or(Value::Null);
            let render = props.get("renderData").cloned().unwrap_or(Value::Null);
            let group = props
                .get("currentGroup")
                .filter(|v| !v.is_null())
                .cloned()
                .or_else(|| render.get("currentGroup").cloned())
                .unwrap_or(Value::Null);
            let group_updated = group
                .get("metadata")
                .and_then(|m| m.get("updatedAt"))
                .cloned()
                .filter(|v| !v.is_null())
                .or_else(|| group.get("updatedAt").cloned());
            let group_name_str = group_name(&group).unwrap_or_else(|| slug.clone());

            emit_and_track(
                &mut out,
                &mut live_by_group,
                &mut seen,
                self.config.checkpoint_every,
                &self.config.include_kinds,
                &serde_json::json!({
                    "_kind": "community",
                    "slug": slug,
                    "groupName": group_name_str,
                    "updatedAt": group_updated,
                }),
                &group_name_str,
            );
            community_complete
                .entry(group_name_str.clone())
                .or_insert(true);
            skipped_courses.entry(group_name_str.clone()).or_insert(0);

            let courses = parse_courses(&next_data);
            if courses.is_empty() {
                eprintln!(
                    "skool: found 0 courses for {slug} (layout change, or a genuinely empty \
                     community)."
                );
            }
            for course in &courses {
                let course_slug = course
                    .get("slug")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| course.get("id").and_then(value_to_id_string));
                let Some(course_slug) = course_slug else {
                    continue;
                };
                if !course_selected(&self.config.courses, &slug, course) {
                    *skipped_courses.entry(group_name_str.clone()).or_insert(0) += 1;
                    continue;
                }
                course_pairs.push((slug.clone(), course_slug.clone()));
                course_lookup.insert(
                    (slug.clone(), course_slug),
                    (group_name_str.clone(), course.clone()),
                );
            }
        }

        // -- phase 2: the lesson tree for every selected course ------
        let courses_result: Vec<Value> = if course_pairs.is_empty() {
            Vec::new()
        } else {
            let payload = serde_json::json!(course_pairs
                .iter()
                .map(|(s, c)| vec![s.clone(), c.clone()])
                .collect::<Vec<_>>());
            match acquire("courses", &session_dir, self.config.headless, &payload) {
                Ok(r) => r
                    .get("courses")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            }
        };

        let mut returned_pairs: HashSet<(String, String)> = HashSet::new();
        for entry in &courses_result {
            let (Some(slug), Some(course_slug)) = (
                entry.get("slug").and_then(Value::as_str),
                entry.get("course_slug").and_then(Value::as_str),
            ) else {
                continue;
            };
            let key = (slug.to_string(), course_slug.to_string());
            returned_pairs.insert(key.clone());
            let Some((group_name_str, course_record)) = course_lookup.get(&key) else {
                continue;
            };
            let group_name_str = group_name_str.clone();
            let course_record = course_record.clone();
            let next_data = entry.get("next_data").cloned().unwrap_or(Value::Null);
            let course_name = course_record
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| course_slug.to_string());

            emit_and_track(
                &mut out,
                &mut live_by_group,
                &mut seen,
                self.config.checkpoint_every,
                &self.config.include_kinds,
                &serde_json::json!({
                    "_kind": "course",
                    "courseName": course_name,
                    "courseImageUrl": course_record.get("coverImageUrl").cloned().unwrap_or(Value::Null),
                    "updatedAt": course_record.get("updatedAt").cloned().unwrap_or(Value::Null),
                    "hasAccess": course_record.get("hasAccess").cloned().unwrap_or(Value::Null),
                    "privacy": course_record.get("privacy").cloned().unwrap_or(Value::Null),
                    "numModules": course_record.get("numModules").cloned().unwrap_or(Value::Null),
                    "_group_slug": slug,
                    "groupName": group_name_str,
                }),
                &group_name_str,
            );

            for mut lesson in parse_lessons(&next_data) {
                if let Value::Object(map) = &mut lesson {
                    map.insert("_kind".to_string(), Value::String("lesson".to_string()));
                    map.insert(
                        "_group_name".to_string(),
                        Value::String(group_name_str.clone()),
                    );
                    map.insert(
                        "_course_name".to_string(),
                        Value::String(course_name.clone()),
                    );
                }
                emit_and_track(
                    &mut out,
                    &mut live_by_group,
                    &mut seen,
                    self.config.checkpoint_every,
                    &self.config.include_kinds,
                    &lesson,
                    &group_name_str,
                );
            }
        }
        for pair in &course_pairs {
            if !returned_pairs.contains(pair) {
                if let Some((group_name_str, _)) = course_lookup.get(pair) {
                    community_complete.insert(group_name_str.clone(), false);
                }
            }
        }

        out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: serde_json::json!({"items_seen": seen}),
            },
            note: "final".to_string(),
        })));

        let mut groups: Vec<&String> = live_by_group.keys().collect();
        groups.sort();
        let mut incomplete: Vec<&str> = Vec::new();
        for group in groups {
            let complete = community_complete.get(group).copied().unwrap_or(false);
            let skipped = skipped_courses.get(group).copied().unwrap_or(0);
            if complete && skipped == 0 {
                out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker {
                    live_ids: live_by_group[group].clone(),
                    scope: format!("tag:{group}"),
                })));
            } else {
                incomplete.push(group.as_str());
            }
        }
        if !incomplete.is_empty() {
            eprintln!(
                "skool: partial enumeration for {} (communities/courses filter, or a course \
                 failed to load) — deletion detection skipped there",
                incomplete.join(", ")
            );
        }

        Box::new(out.into_iter())
    }
}

fn emit_and_track(
    out: &mut Vec<Result<FetchEvent, ConnectorError>>,
    live_by_group: &mut HashMap<String, HashSet<String>>,
    seen: &mut u32,
    checkpoint_every: u32,
    include_kinds: &[String],
    raw: &Value,
    group: &str,
) {
    let Some(item) = to_item(raw) else {
        return;
    };
    live_by_group
        .entry(group.to_string())
        .or_default()
        .insert(item.external_id().to_string());
    if !include_kinds.is_empty() && !include_kinds.contains(&item.item_kind) {
        return;
    }
    out.push(Ok(FetchEvent::Item(item)));
    *seen += 1;
    if checkpoint_every > 0 && seen.is_multiple_of(checkpoint_every) {
        out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: serde_json::json!({"items_seen": seen}),
            },
            note: format!("after {seen} items"),
        })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(session_dir: Option<&str>, downloads_dir: Option<PathBuf>) -> RunContext {
        let mut store = HashMap::new();
        if let Some(d) = session_dir {
            store.insert("SKOOL_SESSION_DIR".to_string(), d.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "skool".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(
                store,
                vec![
                    "SKOOL_SESSION_DIR".to_string(),
                    "YOUTUBE_COOKIES_FILE".to_string(),
                    "GITHUB_TOKEN".to_string(),
                ],
            ),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: downloads_dir,
            items_failed: 0,
            cancel: None,
            http: None,
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-connector-skool-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fetch_with_an_undeclared_video_cookies_file_env_is_a_config_error() {
        let config = SkoolConfig {
            video_cookies_file_env: Some("SOME_OTHER_VAR".to_string()),
            ..Default::default()
        };
        let mut connector = SkoolConnector::new(config);
        let ctx = ctx_with(None, None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_without_the_session_dir_secret_set_is_an_auth_error() {
        let mut connector = SkoolConnector::new(SkoolConfig::default());
        let ctx = ctx_with(None, None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn fetch_with_a_nonexistent_session_dir_is_a_config_error() {
        let mut connector = SkoolConnector::new(SkoolConfig::default());
        let ctx = ctx_with(Some("/nonexistent/path/that/should/not/exist"), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_without_a_downloads_root_is_a_config_error() {
        let dir = temp_dir("session-only");
        let mut connector = SkoolConnector::new(SkoolConfig::default());
        let ctx = ctx_with(Some(&dir.to_string_lossy()), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    /// With a valid (but real-session-less) directory, `fetch()` now
    /// actually runs the acquisition script (#188) instead of
    /// returning a static "blocked" error. It still can't succeed in
    /// a sandbox with no live Playwright/Skool session, but exactly
    /// what it fails with is environment-dependent (see
    /// `dbs-connector-reddit`'s identical test for the same
    /// reasoning) — so this only asserts a single error result.
    #[test]
    fn fetch_with_everything_valid_but_no_real_session_fails_cleanly() {
        let session = temp_dir("valid-session");
        let downloads = temp_dir("valid-downloads");
        let mut connector = SkoolConnector::new(SkoolConfig::default());
        let ctx = ctx_with(Some(&session.to_string_lossy()), Some(downloads));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1, "{result:?}");
        assert!(result[0].is_err(), "{result:?}");
    }

    #[test]
    fn group_name_prefers_display_name_then_name_then_top_level_name() {
        let g1 = serde_json::json!({"metadata": {"displayName": "Rust Devs", "name": "rustdevs"}, "name": "fallback"});
        assert_eq!(group_name(&g1), Some("Rust Devs".to_string()));
        let g2 = serde_json::json!({"metadata": {"name": "rustdevs"}, "name": "fallback"});
        assert_eq!(group_name(&g2), Some("rustdevs".to_string()));
        let g3 = serde_json::json!({"name": "fallback"});
        assert_eq!(group_name(&g3), Some("fallback".to_string()));
    }

    #[test]
    fn deep_find_returns_the_first_bfs_match() {
        let tree = serde_json::json!({"a": {"b": {"allGroups": "deep"}}, "allGroups": "shallow"});
        assert_eq!(
            deep_find(&tree, "allGroups"),
            Some(Value::String("shallow".to_string()))
        );
        let tree2 = serde_json::json!({"a": [{"b": {"target": "found"}}]});
        assert_eq!(
            deep_find(&tree2, "target"),
            Some(Value::String("found".to_string()))
        );
        assert_eq!(deep_find(&tree2, "nope"), None);
    }

    #[test]
    fn json_field_parses_an_embedded_json_string_and_passes_through_non_strings() {
        assert_eq!(
            json_field(&Value::String(r#"{"url":"x"}"#.to_string())),
            serde_json::json!({"url": "x"})
        );
        assert_eq!(
            json_field(&Value::String("plain text".to_string())),
            Value::String("plain text".to_string())
        );
        assert_eq!(json_field(&Value::Bool(true)), Value::Bool(true));
    }

    #[test]
    fn lesson_fields_extracts_video_link_from_a_video_object() {
        let node = serde_json::json!({
            "metadata": {"video": r#"{"url": "https://stream.example.com/x.m3u8"}"#},
        });
        let fields = lesson_fields(&node);
        assert_eq!(fields["videoLink"], "https://stream.example.com/x.m3u8");
    }

    #[test]
    fn lesson_fields_marks_a_link_only_resource_as_external() {
        let node = serde_json::json!({
            "metadata": {"resources": r#"[{"link": "https://example.com/repo"}]"#},
        });
        let fields = lesson_fields(&node);
        let resources = fields["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0]["downloadUrl"], "https://example.com/repo");
        assert_eq!(resources[0]["isExternal"], true);
    }

    #[test]
    fn find_lesson_node_matches_by_id_under_course_props() {
        let next_data = serde_json::json!({
            "props": {"pageProps": {"course": {"children": [
                {"id": "999", "metadata": {"title": "wrong"}},
                {"id": "42", "metadata": {"title": "Lesson 42"}},
            ]}}}
        });
        let found = find_lesson_node(&next_data, "42").unwrap();
        assert_eq!(found["metadata"]["title"], "Lesson 42");
        assert!(find_lesson_node(&next_data, "").is_none());
        assert!(find_lesson_node(&next_data, "does-not-exist").is_none());
    }

    #[test]
    fn course_selected_matches_scoped_and_unscoped_selectors() {
        let course = serde_json::json!({"title": "Rust Basics", "slug": "rust-basics"});
        assert!(course_selected(&[], "my-community", &course));
        assert!(course_selected(
            &["rust basics".to_string()],
            "my-community",
            &course
        ));
        assert!(course_selected(
            &["my-community/rust-basics".to_string()],
            "my-community",
            &course
        ));
        assert!(!course_selected(
            &["other-community/rust-basics".to_string()],
            "my-community",
            &course
        ));
        assert!(!course_selected(
            &["unrelated".to_string()],
            "my-community",
            &course
        ));
    }

    #[test]
    fn mux_hls_url_builds_the_stream_url_when_ids_match() {
        let next_data = serde_json::json!({
            "props": {"pageProps": {"video": {
                "id": "vid1", "playbackId": "abc123", "playbackToken": "tok456"
            }}}
        });
        let video_id = Value::String("vid1".to_string());
        assert_eq!(
            mux_hls_url(&next_data, &video_id),
            Some("https://stream.video.skool.com/abc123.m3u8?token=tok456".to_string())
        );
        let wrong_id = Value::String("vid2".to_string());
        assert_eq!(mux_hls_url(&next_data, &wrong_id), None);
    }

    #[test]
    fn url_host_matches_handles_subdomains_and_rejects_lookalikes() {
        let hosts = vec!["vimeo.com".to_string()];
        assert!(url_host_matches("https://vimeo.com/123", &hosts));
        assert!(url_host_matches("https://player.vimeo.com/123", &hosts));
        assert!(url_host_matches("https://www.vimeo.com/123", &hosts));
        assert!(!url_host_matches("https://notvimeo.com/123", &hosts));
        assert!(!url_host_matches("not a url", &hosts));
    }

    #[test]
    fn classify_video_error_detects_permanent_failures() {
        assert_eq!(classify_video_error("Video unavailable"), "unavailable");
        assert_eq!(classify_video_error("This video is private"), "unavailable");
        assert_eq!(
            classify_video_error("Sign in to confirm you're not a bot"),
            "failed"
        );
        assert_eq!(classify_video_error("connection reset"), "failed");
    }

    #[test]
    fn parse_memberships_dedupes_by_slug() {
        let next_data = serde_json::json!({
            "props": {"pageProps": {"self": {"allGroups": [
                {"name": "rustdevs", "metadata": {"displayName": "Rust Devs"}},
                {"name": "rustdevs", "metadata": {"displayName": "Rust Devs (dup)"}},
                {"name": "gophers", "metadata": {"displayName": "Gophers"}},
            ]}}}
        });
        let memberships = parse_memberships(&next_data);
        assert_eq!(memberships.len(), 2);
        assert_eq!(memberships[0]["slug"], "rustdevs");
        assert_eq!(memberships[0]["displayName"], "Rust Devs");
    }

    #[test]
    fn parse_courses_maps_tri_state_has_access() {
        let next_data = serde_json::json!({
            "props": {"pageProps": {"allCourses": [
                {"id": "1", "name": "c1", "metadata": {"title": "Course One", "hasAccess": 1}},
                {"id": "2", "name": "c2", "metadata": {"title": "Course Two", "hasAccess": 0}},
                {"id": "3", "name": "c3", "metadata": {"title": "Course Three"}},
            ]}}
        });
        let courses = parse_courses(&next_data);
        assert_eq!(courses.len(), 3);
        assert_eq!(courses[0]["hasAccess"], true);
        assert_eq!(courses[1]["hasAccess"], false);
        assert_eq!(courses[2]["hasAccess"], Value::Null);
    }

    #[test]
    fn parse_lessons_splits_modules_from_bare_lessons() {
        let next_data = serde_json::json!({
            "props": {"pageProps": {"course": {"children": [
                {
                    "metadata": {"title": "Module One"},
                    "children": [
                        {"id": "10", "metadata": {"title": "Lesson A"}},
                        {"id": "11", "metadata": {"title": "Lesson B"}},
                    ],
                },
                {"id": "20", "metadata": {"title": "Bare Lesson"}},
            ]}}}
        });
        let lessons = parse_lessons(&next_data);
        assert_eq!(lessons.len(), 3);
        assert_eq!(lessons[0]["moduleTitle"], "Module One");
        assert_eq!(lessons[0]["title"], "Lesson A");
        assert_eq!(lessons[1]["moduleTitle"], "Module One");
        assert_eq!(lessons[2]["moduleTitle"], Value::Null);
        assert_eq!(lessons[2]["title"], "Bare Lesson");
    }

    #[test]
    fn slug_from_community_extracts_the_path_segment() {
        assert_eq!(
            slug_from_community("https://www.skool.com/rustdevs/about"),
            "rustdevs"
        );
        assert_eq!(slug_from_community("skool.com/rustdevs"), "rustdevs");
        assert_eq!(slug_from_community("rustdevs"), "rustdevs");
        assert_eq!(slug_from_community("/rustdevs/"), "rustdevs");
    }

    #[test]
    fn community_item_maps_slug_and_display_name() {
        let raw = serde_json::json!({"_kind": "community", "slug": "rustdevs", "groupName": "Rust Devs", "updatedAt": "2024-06-01T00:00:00Z"});
        let item = community_item(&raw).unwrap();
        assert_eq!(item.external_id(), "community:rustdevs");
        assert_eq!(item.title.as_deref(), Some("Rust Devs"));
    }

    #[test]
    fn course_item_scopes_the_external_id_to_its_community() {
        let raw = serde_json::json!({
            "_kind": "course", "courseName": "Rust Basics", "_group_slug": "rustdevs",
            "groupName": "Rust Devs", "courseImageUrl": "https://img.example.com/x.jpg",
        });
        let item = course_item(&raw).unwrap();
        assert_eq!(item.external_id(), "course:rustdevs/Rust Basics");
        assert_eq!(item.tags, vec!["Rust Devs".to_string()]);
        assert_eq!(item.media.len(), 1);
    }

    #[test]
    fn lesson_item_prefers_a_downloaded_video_path_over_the_link() {
        let raw = serde_json::json!({
            "_kind": "lesson", "lessonId": "42", "title": "Lesson 42",
            "videoLink": "https://stream.example.com/x.m3u8",
            "_video_path": "/downloads/rustdevs/rust-basics/01 - Lesson 42/01 - Lesson 42.mp4",
            "_group_name": "Rust Devs", "_course_name": "Rust Basics", "moduleTitle": "Module One",
        });
        let item = lesson_item(&raw).unwrap();
        assert_eq!(item.external_id(), "42");
        assert_eq!(
            item.tags,
            vec![
                "Rust Devs".to_string(),
                "Rust Basics".to_string(),
                "Module One".to_string()
            ]
        );
        let video_media = item.media.iter().find(|m| m.kind == "video").unwrap();
        assert_eq!(
            video_media.url,
            "/downloads/rustdevs/rust-basics/01 - Lesson 42/01 - Lesson 42.mp4"
        );
    }

    #[test]
    fn lesson_item_suppresses_an_unavailable_videos_link() {
        let raw = serde_json::json!({
            "_kind": "lesson", "lessonId": "42", "videoLink": "https://stream.example.com/x.m3u8",
            "videoUnavailable": true,
        });
        let item = lesson_item(&raw).unwrap();
        assert!(!item.media.iter().any(|m| m.kind == "video"));
    }

    #[test]
    fn to_item_dispatches_by_kind() {
        assert!(to_item(&serde_json::json!({"_kind": "community", "slug": "x"})).is_some());
        assert!(to_item(&serde_json::json!({"_kind": "course", "courseName": "x"})).is_some());
        assert!(to_item(&serde_json::json!({"_kind": "lesson", "lessonId": "1"})).is_some());
        assert!(to_item(&serde_json::json!({"_kind": "_community_complete"})).is_none());
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = SkoolConnector::new(SkoolConfig::default());
        assert_eq!(connector.type_name(), "skool");
        assert_eq!(
            connector.secret_keys(),
            &[
                "SKOOL_SESSION_DIR".to_string(),
                "YOUTUBE_COOKIES_FILE".to_string(),
                "GITHUB_TOKEN".to_string(),
            ]
        );
        assert!(!connector.wants_managed_http());
        assert!(connector.needs_playwright_browser());
        assert_eq!(connector.item_kinds().len(), 3);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(connector.capabilities().supports_full_enumeration);
        assert_eq!(connector.capabilities().concurrency, "serial");
        assert_eq!(connector.volatile_fields(), &["updatedAt".to_string()]);
        let capture = connector.auth_capture().unwrap();
        assert_eq!(capture.kind, "browser_session");
        assert_eq!(capture.secret_key, "SKOOL_SESSION_DIR");
    }

    // -- acquire_using: exercised against a fake stub "interpreter"
    // (mirrors dbs-connector-reddit's identical convention) so the
    // JSON result contract between the Python script and this Rust
    // code is tested without needing real Python, Playwright, or
    // network access. -----------------------------------------------

    fn write_stub_script(dir: &std::path::Path, stdout: &str, exit_code: i32) -> PathBuf {
        let path = dir.join("stub.sh");
        let body = format!("#!/bin/sh\ncat <<'EOF'\n{stdout}\nEOF\nexit {exit_code}\n");
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn acquire_using_parses_a_successful_communities_result() {
        let dir = temp_dir("acquire-communities-ok");
        let script = write_stub_script(
            &dir,
            r#"{"ok": true, "communities": [{"slug": "chase-ai", "next_data": {"a": 1}}]}"#,
            0,
        );
        let result = acquire_using(
            "/bin/sh",
            &script,
            "communities",
            "/some/dir",
            true,
            &serde_json::json!([]),
            Duration::from_secs(5),
        )
        .unwrap();
        let communities = result.get("communities").and_then(Value::as_array).unwrap();
        assert_eq!(communities.len(), 1);
        assert_eq!(communities[0]["slug"], "chase-ai");
    }

    #[test]
    fn acquire_using_parses_a_successful_courses_result() {
        let dir = temp_dir("acquire-courses-ok");
        let script = write_stub_script(
            &dir,
            r#"{"ok": true, "courses": [{"slug": "chase-ai", "course_slug": "c1", "next_data": {}}]}"#,
            0,
        );
        let result = acquire_using(
            "/bin/sh",
            &script,
            "courses",
            "/some/dir",
            true,
            &serde_json::json!([["chase-ai", "c1"]]),
            Duration::from_secs(5),
        )
        .unwrap();
        let courses = result.get("courses").and_then(Value::as_array).unwrap();
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0]["course_slug"], "c1");
    }

    fn acquire_with_error_kind(dir: &std::path::Path, kind: &str) -> Result<Value, ConnectorError> {
        let script = write_stub_script(
            dir,
            &format!(r#"{{"ok": false, "kind": "{kind}", "message": "boom"}}"#),
            1,
        );
        acquire_using(
            "/bin/sh",
            &script,
            "communities",
            "/some/dir",
            true,
            &serde_json::json!([]),
            Duration::from_secs(5),
        )
    }

    #[test]
    fn acquire_using_maps_each_error_kind_to_the_matching_connector_error() {
        let dir = temp_dir("acquire-errors");
        assert!(matches!(
            acquire_with_error_kind(&dir, "auth"),
            Err(ConnectorError::Auth(_))
        ));
        assert!(matches!(
            acquire_with_error_kind(&dir, "config"),
            Err(ConnectorError::Config(_))
        ));
        assert!(matches!(
            acquire_with_error_kind(&dir, "rate_limited"),
            Err(ConnectorError::RateLimited(_))
        ));
        assert!(matches!(
            acquire_with_error_kind(&dir, "something_unrecognized"),
            Err(ConnectorError::Transient(_))
        ));
    }

    #[test]
    fn acquire_using_passes_mode_and_payload_through_as_positional_arguments() {
        let dir = temp_dir("acquire-args");
        // The payload arg is JSON (so it contains literal quotes) --
        // round-tripping it through a shell-echoed JSON string would
        // fight the shell's own quoting, so the stub script instead
        // writes each raw argv element to its own file for the test
        // to read back directly.
        let args_out = dir.join("args.txt");
        let script = dir.join("dump_args.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n%s\\n' \"$1\" \"$2\" \"$3\" \"$4\" > \"{}\"\n\
                 echo '{{\"ok\": true, \"communities\": []}}'\n",
                args_out.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        acquire_using(
            "/bin/sh",
            &script,
            "communities",
            "/my/session/dir",
            false,
            &serde_json::json!(["a", "b"]),
            Duration::from_secs(5),
        )
        .unwrap();
        let recorded = std::fs::read_to_string(&args_out).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(
            lines,
            vec!["communities", "/my/session/dir", "false", r#"["a","b"]"#]
        );
    }

    #[test]
    fn acquire_using_treats_unparseable_output_as_transient() {
        let dir = temp_dir("acquire-garbage");
        let script = write_stub_script(&dir, "not json at all", 1);
        let result = acquire_using(
            "/bin/sh",
            &script,
            "communities",
            "/some/dir",
            true,
            &serde_json::json!([]),
            Duration::from_secs(5),
        );
        match result {
            Err(ConnectorError::Transient(msg)) => assert!(msg.contains("unparseable"), "{msg}"),
            other => panic!("expected a Transient error, got {other:?}"),
        }
    }

    // -- emit_and_track: the pure per-record bookkeeping fetch() uses,
    // tested directly. ------------------------------------------------

    #[test]
    fn emit_and_track_records_every_id_in_live_by_group_even_when_excluded() {
        let mut out = Vec::new();
        let mut live_by_group = HashMap::new();
        let mut seen = 0u32;
        emit_and_track(
            &mut out,
            &mut live_by_group,
            &mut seen,
            200,
            &["lesson".to_string()], // excludes "course"
            &serde_json::json!({"_kind": "course", "courseName": "x"}),
            "chase-ai",
        );
        assert!(out.is_empty());
        assert_eq!(seen, 0);
        assert!(live_by_group["chase-ai"].contains("course:x"));
    }

    #[test]
    fn emit_and_track_checkpoints_every_n_yielded_items() {
        let mut out = Vec::new();
        let mut live_by_group = HashMap::new();
        let mut seen = 0u32;
        for i in 0..4 {
            emit_and_track(
                &mut out,
                &mut live_by_group,
                &mut seen,
                2,
                &[],
                &serde_json::json!({"_kind": "lesson", "lessonId": i.to_string()}),
                "chase-ai",
            );
        }
        assert_eq!(seen, 4);
        let checkpoints = out
            .iter()
            .filter(|e| matches!(e, Ok(FetchEvent::Checkpoint(_))))
            .count();
        assert_eq!(checkpoints, 2);
    }
}
