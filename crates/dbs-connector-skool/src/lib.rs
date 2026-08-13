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
//! **Acquisition is blocked on issue #99** (the shared Playwright
//! launch helper), the same gap `dbs-connector-reddit` (#96) is
//! blocked on. `fetch()` performs every check that doesn't need a
//! browser (`video_cookies_file_env` is declared correctly, the
//! `SKOOL_SESSION_DIR` secret is set, the session directory actually
//! exists, a downloads folder resolves) before returning a clear
//! [`ConnectorError::Config`] pointing at #99.
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
//!
//! Deliberately **not** ported for the same reason as the rest of
//! acquisition: resource-file downloads, the `yt-dlp`-driven video
//! download once a URL is found, the `.meta.json` sidecar / resume
//! pipeline, directory-naming and note-writing, and GitHub-zip
//! archiving of code lessons link to. All of these are pipeline
//! mechanics with zero reachable callers until #99 exists; porting
//! them now would be speculative work with no way to exercise them
//! against the real site.
//!
//! **Not wired up:** same boundary as `dbs-connector-raindrop` (#85)
//! through `dbs-connector-reddit` (#96) for the registry run/stream
//! bridge — and, separately, blocked on issue #99 for acquisition
//! specifically, as described above.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use dbs_connector_support::tiptap_markdown;
use dbs_core::parse_iso;
use dbs_core::{
    AuthCapture, BackupItem, Capabilities, Connector, ConnectorError, FetchEvent, ItemKind,
    MediaRef, RunContext,
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

// Everything below is pure — no browser, no HTTP — and unreachable
// from `fetch()` yet (see the module doc-comment), so kept alive by
// tests via `#[allow(dead_code)]` until issue #99's future
// acquisition step can call into it for real.

fn value_to_id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A community's display name: `metadata.displayName`, falling back
/// to `metadata.name`, falling back to the group's own `name`.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
fn json_field(value: &Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| value.clone()),
        other => other.clone(),
    }
}

/// Decodes one lesson node's video/resources/description out of its
/// (possibly JSON-encoded) `metadata`, normalizing resources that
/// carry a bare `link` (no `downloadUrl`) into external references.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
fn slug_from_community(community: &str) -> String {
    if let Some(idx) = community.find("skool.com/") {
        let rest = &community[idx + "skool.com/".len()..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    community.trim_matches(|c| c == '/' || c == ' ').to_string()
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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
#[allow(dead_code)]
fn to_item(raw: &Value) -> Option<BackupItem> {
    match raw.get("_kind").and_then(|v| v.as_str()) {
        Some("community") => community_item(raw),
        Some("course") => course_item(raw),
        Some("lesson") => lesson_item(raw),
        _ => None,
    }
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

        // The rest of acquisition — launching the captured session as
        // a Chromium context, reading each page's __NEXT_DATA__ blob,
        // and visiting every lesson's own page to sniff its video and
        // resources — needs the shared Playwright launch helper this
        // port doesn't have yet (see the module doc-comment).
        out.push(Err(ConnectorError::Config(
            "skool: community/course/lesson acquisition needs a Playwright launch helper this \
             port doesn't have yet (gap-analysis.md's Connectors cluster, issue #99) — the \
             session directory and downloads folder are valid; this connector will be wired up \
             once #99 lands."
                .to_string(),
        )));

        Box::new(out.into_iter())
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

    #[test]
    fn fetch_with_everything_valid_is_blocked_pending_the_playwright_helper() {
        let session = temp_dir("valid-session");
        let downloads = temp_dir("valid-downloads");
        let mut connector = SkoolConnector::new(SkoolConfig::default());
        let ctx = ctx_with(Some(&session.to_string_lossy()), Some(downloads));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        match &result[0] {
            Err(ConnectorError::Config(msg)) => assert!(msg.contains("issue #99"), "{msg}"),
            other => panic!("expected a Config error mentioning issue #99, got {other:?}"),
        }
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
}
