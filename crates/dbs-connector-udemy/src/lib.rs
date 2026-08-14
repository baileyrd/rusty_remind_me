//! Udemy connector: backs up your enrolled courses and their
//! curricula (issue #95). Mirrors `dbs.connectors.udemy` in
//! baileyrd/Daily-Backup-System.
//!
//! Udemy has no official public API for learners, but the web app's
//! own REST surface (`/api-2.0`) is stable and well-understood. Auth
//! is the `access_token` cookie from a logged-in browser — set
//! `UDEMY_ACCESS_TOKEN` to its value; it's sent both as a Bearer
//! header and as a cookie, matching the web client. Udemy fronts the
//! API with Cloudflare, which blocks obviously non-browser clients,
//! so requests carry a desktop browser User-Agent.
//!
//! Two layers are stored:
//! - **course** — one item per enrolled course.
//! - **lecture** / **quiz** — one item per curriculum entry, walked
//!   per course. Article lectures keep their full HTML in `body`;
//!   each entry's chapter title and course are injected into `raw`
//!   under `_dbs_`-prefixed keys; downloadable supplementary assets
//!   become `MediaRef` entries.
//!
//! `download_videos = true` additionally downloads each video lecture
//! via the `yt-dlp` binary (needs `UDEMY_COOKIES_FILE`, a Netscape
//! cookies.txt export — yt-dlp needs the full cookie jar, not just
//! the one token). Downloads are idempotent (existing file wins) and
//! best-effort: a failed or DRM-protected lecture is logged and the
//! run moves on.
//!
//! Like `vimeo` this is a full-enumeration connector: no server-side
//! delta, so every run walks everything and one
//! [`ReconcileMarker`][dbs_core::ReconcileMarker] soft-deletes courses
//! you've since been unenrolled from. If any single course's
//! curriculum fails to load, the run continues but is a **partial
//! enumeration** — the marker is withheld so the missing course's
//! lectures can't be falsely swept; deletion detection resumes on the
//! next clean run.
//!
//! `completion_ratio`/`last_accessed_time` churn every time you watch
//! anything; they're `volatile_fields` so progress ticks never spawn
//! revisions.
//!
//! **Subprocess, not a library call**, same as `vimeo` (#94): per
//! this port's round-1 decision, `yt-dlp` is shelled out to as a
//! binary. Stall detection uses
//! [`dbs_connector_support::run_with_watchdog`] with a heartbeat fed
//! by the subprocess's own stdout lines.
//!
//! Reachable from a real `dbs backup udemy` run since #164's
//! `dbs-connector-udemy` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses; the download path
//! is tested against a fake `yt-dlp` script on disk.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbs_connector_support::{run_with_watchdog, WatchdogError};
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, MediaRef, ReconcileMarker, RunContext,
};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://www.udemy.com";
// Cloudflare blocks default non-browser UAs; look like the browser
// whose cookie we carry.
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                   Chrome/126.0 Safari/537.36";
const COURSE_FIELDS: &str = "id,title,url,image_480x270,num_lectures,completion_ratio,\
                              last_accessed_time,created,published_title";
const CURRICULUM_PARAMS: &[(&str, &str)] = &[
    ("page_size", "200"),
    (
        "fields[lecture]",
        "id,title,object_index,asset,supplementary_assets,created",
    ),
    ("fields[chapter]", "id,title,object_index"),
    (
        "fields[asset]",
        "id,asset_type,filename,time_estimation,body,download_urls,external_url",
    ),
    ("fields[quiz]", "id,title,object_index"),
];

#[derive(Debug, Clone)]
pub struct UdemyConfig {
    pub page_size: u32,
    /// Limit the backup to these courses (numeric ids or published
    /// slugs). Empty = every enrolled course.
    pub course_filter: Vec<String>,
    /// Download video lectures with `yt-dlp` (needs
    /// `UDEMY_COOKIES_FILE`). DRM-protected courses are skipped with
    /// a warning.
    pub download_videos: bool,
    /// `yt-dlp` format selector.
    pub video_format: String,
    /// Abandon a video download after this many seconds without
    /// progress; the lecture is skipped with a warning. 0 = no cap.
    pub download_timeout: u64,
}

impl Default for UdemyConfig {
    fn default() -> Self {
        Self {
            page_size: 100,
            course_filter: Vec::new(),
            download_videos: false,
            video_format: "best".to_string(),
            download_timeout: 900,
        }
    }
}

pub struct UdemyConnector {
    config: UdemyConfig,
    base_url: String,
    yt_dlp_bin: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl UdemyConnector {
    pub fn new(config: UdemyConfig) -> Self {
        Self {
            config,
            base_url: DEFAULT_BASE_URL.to_string(),
            yt_dlp_bin: "yt-dlp".to_string(),
            secret_keys: vec![
                "UDEMY_ACCESS_TOKEN".to_string(),
                "UDEMY_COOKIES_FILE".to_string(),
            ],
            item_kinds: vec![
                ItemKind {
                    name: "course".to_string(),
                    display_name: "Course".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "lecture".to_string(),
                    display_name: "Lecture".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "quiz".to_string(),
                    display_name: "Quiz".to_string(),
                    description: String::new(),
                },
            ],
            // Watch-progress fields churn on every visit; never
            // revision on them alone.
            volatile_fields: vec![
                "completion_ratio".to_string(),
                "last_accessed_time".to_string(),
            ],
        }
    }

    /// Overrides the base URL (default `https://www.udemy.com`) —
    /// for tests to point at a local mock server.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Overrides the `yt-dlp` binary name/path (default `"yt-dlp"`)
    /// — for tests to point at a fake script on disk.
    pub fn with_yt_dlp_bin(mut self, bin: impl Into<String>) -> Self {
        self.yt_dlp_bin = bin.into();
        self
    }

    fn get_json(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        url: &str,
        params: Option<&[(&str, String)]>,
    ) -> Result<Value, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, url, |b| {
                let b = b
                    .bearer_auth(token)
                    .header("Cookie", format!("access_token={token}"))
                    .header("User-Agent", UA)
                    .header("Accept", "application/json");
                match params {
                    Some(p) => b.query(p),
                    None => b,
                }
            })
            .map_err(classify_api_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    /// Yields results across Udemy's `next`-linked pages.
    fn paginate(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        first_url: &str,
        first_params: Option<&[(&str, String)]>,
    ) -> Result<Vec<Value>, ConnectorError> {
        let mut out = Vec::new();
        let mut url = Some(first_url.to_string());
        let mut first = true;
        while let Some(u) = url.take() {
            let payload =
                self.get_json(http, token, &u, if first { first_params } else { None })?;
            first = false;
            let results = payload
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            out.extend(results.into_iter().filter(|r| r.is_object()));
            url = payload
                .get("next")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        Ok(out)
    }

    fn list_courses(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
    ) -> Result<Vec<Value>, ConnectorError> {
        let params = [
            ("page_size", self.config.page_size.to_string()),
            ("fields[course]", COURSE_FIELDS.to_string()),
        ];
        let all = self.paginate(
            http,
            token,
            &format!("{}/api-2.0/users/me/subscribed-courses/", self.base_url),
            Some(&params),
        )?;
        let wanted: HashSet<&str> = self
            .config
            .course_filter
            .iter()
            .map(String::as_str)
            .collect();
        Ok(all
            .into_iter()
            .filter(|c| {
                let Some(id) = value_to_id_string(c.get("id")) else {
                    return false;
                };
                if wanted.is_empty() {
                    return true;
                }
                let slug = c.get("published_title").and_then(|v| v.as_str());
                wanted.contains(id.as_str()) || slug.is_some_and(|s| wanted.contains(s))
            })
            .collect())
    }

    fn list_curriculum(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        course_id: &str,
    ) -> Result<Vec<Value>, ConnectorError> {
        let params: Vec<(&str, String)> = CURRICULUM_PARAMS
            .iter()
            .map(|(k, v)| (*k, v.to_string()))
            .collect();
        self.paginate(
            http,
            token,
            &format!(
                "{}/api-2.0/courses/{course_id}/subscriber-curriculum-items/",
                self.base_url
            ),
            Some(&params),
        )
    }

    /// Download one lecture video; idempotent and best-effort.
    fn maybe_download_video(
        &self,
        ctx: &RunContext,
        course: &Value,
        entry: &Value,
    ) -> Option<PathBuf> {
        let download_dir = ctx.download_dir.as_ref()?;
        let course_id = value_to_id_string(course.get("id")).unwrap_or_default();
        let slug = course
            .get("published_title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| course_id.clone());
        let entry_id = value_to_id_string(entry.get("id")).unwrap_or_default();
        let title = safe_name(
            entry
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&entry_id),
        );
        let index = entry
            .get("object_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let folder = download_dir.join(safe_name(&slug));
        let stem = format!("{index:03} - {title}");
        if let Some(existing) = find_by_stem(&folder, &stem) {
            return Some(existing);
        }
        let url = format!("{}/course/{slug}/learn/lecture/{entry_id}", self.base_url);
        self.ytdlp_download(ctx, &url, &folder, &stem)
    }

    fn ytdlp_download(
        &self,
        ctx: &RunContext,
        url: &str,
        folder: &Path,
        stem: &str,
    ) -> Option<PathBuf> {
        let cookiefile = match ctx.secrets.get("UDEMY_COOKIES_FILE") {
            Ok(c) => c.to_string(),
            Err(e) => {
                eprintln!(
                    "udemy: video download failed for {url} ({e}) — DRM-protected lectures \
                     cannot be downloaded"
                );
                return None;
            }
        };
        if !Path::new(&cookiefile).exists() {
            eprintln!(
                "udemy: video download failed for {url} (UDEMY_COOKIES_FILE {cookiefile} does \
                 not exist; export a Netscape cookies.txt from a logged-in browser) — \
                 DRM-protected lectures cannot be downloaded"
            );
            return None;
        }
        if let Err(e) = std::fs::create_dir_all(folder) {
            eprintln!("udemy: video download failed for {url} ({e})");
            return None;
        }
        let outtmpl = folder.join(format!("{stem}.%(ext)s"));
        let ok = run_yt_dlp_download(
            &self.yt_dlp_bin,
            url,
            &cookiefile,
            &self.config.video_format,
            &outtmpl,
            self.config.download_timeout,
        );
        if !ok {
            eprintln!(
                "udemy: video download failed for {url} — DRM-protected lectures cannot be \
                 downloaded"
            );
            return None;
        }
        find_by_stem(folder, stem)
    }
}

fn find_by_stem(folder: &Path, stem: &str) -> Option<PathBuf> {
    std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(stem))
}

fn value_to_id_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn is_video(entry: &Value) -> bool {
    entry
        .get("asset")
        .and_then(|a| a.get("asset_type"))
        .and_then(|v| v.as_str())
        == Some("Video")
}

/// The first download URL of a supplementary asset.
fn first_download(sup: &Value) -> Option<String> {
    let urls = sup.get("download_urls").and_then(|v| v.as_object())?;
    for variants in urls.values() {
        if let Some(arr) = variants.as_array() {
            for v in arr {
                if let Some(file) = v
                    .get("file")
                    .and_then(|f| f.as_str())
                    .filter(|s| !s.is_empty())
                {
                    return Some(file.to_string());
                }
            }
        }
    }
    None
}

fn safe_name(name: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == ' ' || c == '.' || c == '_' || c == '-' {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.' || c == ' ');
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

fn course_item(course: &Value, base_url: &str) -> Option<BackupItem> {
    let id = value_to_id_string(course.get("id"))?;
    let url = course.get("url").and_then(|v| v.as_str());
    let mut media = Vec::new();
    if let Some(image) = course
        .get("image_480x270")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        media.push(MediaRef {
            url: image.to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        });
    }
    let mut item = BackupItem::new(format!("course:{id}"), "course", course.clone()).ok()?;
    item.title = course
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.url = url.map(|u| format!("{base_url}{u}"));
    item.media = media;
    Some(item)
}

fn curriculum_item(
    course: &Value,
    entry: &Value,
    chapter_title: Option<&str>,
    base_url: &str,
) -> Option<BackupItem> {
    let entry_id = value_to_id_string(entry.get("id"))?;
    let kind = if entry.get("_class").and_then(|v| v.as_str()) == Some("quiz") {
        "quiz"
    } else {
        "lecture"
    };
    let mut raw = entry.clone();
    if let Some(obj) = raw.as_object_mut() {
        obj.insert(
            "_dbs_course_id".to_string(),
            course.get("id").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "_dbs_course_title".to_string(),
            course.get("title").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "_dbs_chapter_title".to_string(),
            chapter_title.map(Value::from).unwrap_or(Value::Null),
        );
    }
    let asset = entry.get("asset").cloned().unwrap_or(Value::Null);
    let body = if asset.get("asset_type").and_then(|v| v.as_str()) == Some("Article") {
        asset
            .get("body")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    } else {
        None
    };
    let media = entry
        .get("supplementary_assets")
        .and_then(|v| v.as_array())
        .map(|sups| {
            sups.iter()
                .filter_map(|sup| {
                    first_download(sup).map(|file| MediaRef {
                        url: file,
                        kind: "file".to_string(),
                        filename: sup
                            .get("filename")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        mime: None,
                        data: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let course_id = value_to_id_string(course.get("id")).unwrap_or_default();
    let slug = course
        .get("published_title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| course_id.clone());
    // `lecture:` prefix regardless of `kind`, matching the reference
    // exactly — quizzes share the lecture identity namespace.
    let mut item = BackupItem::new(format!("lecture:{course_id}:{entry_id}"), kind, raw).ok()?;
    item.title = entry
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.url = Some(format!("{base_url}/course/{slug}/learn/lecture/{entry_id}"));
    item.body = body;
    item.tags = [course.get("title").and_then(|v| v.as_str()), chapter_title]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
    item.media = media;
    Some(item)
}

/// Downloads `url` to `folder/{stem}.<ext>` via the `yt-dlp` binary.
/// Returns `true` on success. A stall watchdog abandons the
/// subprocess if no stdout activity arrives within `stall_timeout_secs`.
fn run_yt_dlp_download(
    yt_dlp_bin: &str,
    url: &str,
    cookiefile: &str,
    format: &str,
    outtmpl: &Path,
    stall_timeout_secs: u64,
) -> bool {
    let mut cmd = Command::new(yt_dlp_bin);
    cmd.arg("--quiet")
        .arg("--no-warnings")
        .arg("--newline")
        .arg("-o")
        .arg(outtmpl)
        .arg("--format")
        .arg(format)
        .arg("--cookies")
        .arg(cookiefile)
        .arg("--socket-timeout")
        .arg("30")
        .arg(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("udemy: failed to launch yt-dlp for {url}: {e}");
            return false;
        }
    };
    let stdout = child.stdout.take();
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let reader_handle = stdout.map(|out| {
        let last_activity = Arc::clone(&last_activity);
        std::thread::spawn(move || {
            for _line in BufReader::new(out).lines().map_while(Result::ok) {
                *last_activity.lock().unwrap() = Instant::now();
            }
        })
    });

    let heartbeat_activity = Arc::clone(&last_activity);
    let heartbeat = move || *heartbeat_activity.lock().unwrap();
    let timeout = Duration::from_secs(stall_timeout_secs);
    let heartbeat_ref: Option<&(dyn Fn() -> Instant + Sync)> = if stall_timeout_secs > 0 {
        Some(&heartbeat)
    } else {
        None
    };
    let result = run_with_watchdog(
        move || child.wait(),
        timeout,
        &format!("udemy video {url}"),
        heartbeat_ref,
    );
    if let Some(h) = reader_handle {
        let _ = h.join();
    }

    match result {
        Ok(status) => status.success(),
        Err(WatchdogError::Timeout(t)) => {
            eprintln!("udemy: video download stalled ({url}): {t}");
            false
        }
        Err(WatchdogError::Inner(e)) => {
            eprintln!("udemy: video download failed ({url}): {e}");
            false
        }
        Err(WatchdogError::WorkerPanicked) => {
            eprintln!("udemy: video download failed ({url}): worker thread panicked");
            false
        }
    }
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). Udemy rejects a bad token with 401 or 403; everything
/// else non-retryable is a transient upstream problem.
fn classify_api_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => ConnectorError::Auth(format!(
                "Udemy rejected the access token ({status}) — grab a fresh access_token cookie \
                 from a logged-in browser"
            )),
            Some(status) => ConnectorError::Transient(format!("Udemy API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
    }
}

impl Connector for UdemyConnector {
    fn type_name(&self) -> &str {
        "udemy"
    }

    fn display_name(&self) -> &str {
        "Udemy"
    }

    fn description(&self) -> &str {
        "Backs up your enrolled Udemy courses: metadata, full curriculum (article lectures \
         included), and optionally the lecture videos."
    }

    fn setup_hint(&self) -> &str {
        "Set UDEMY_ACCESS_TOKEN to the value of the `access_token` cookie from a logged-in \
         browser. For download_videos, also export a cookies.txt and set UDEMY_COOKIES_FILE."
    }

    fn secret_keys(&self) -> &[String] {
        &self.secret_keys
    }

    fn wants_managed_http(&self) -> bool {
        true
    }

    fn volatile_fields(&self) -> &[String] {
        &self.volatile_fields
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: false, // enrollment API has no trustworthy delta
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: true,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: true,
            concurrency: "serial".to_string(), // bulk video downloads are resource-heavy
            ..Capabilities::default()
        }
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        let Some(http) = ctx.http.as_ref() else {
            out.push(Err(ConnectorError::Config(
                "Udemy connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        let token = match ctx.secrets.get("UDEMY_ACCESS_TOKEN") {
            Ok(t) => t.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let courses = match self.list_courses(http, &token) {
            Ok(c) => c,
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let mut live_ids = HashSet::new();
        let mut partial = false;
        let mut done = 0u32;
        for course in &courses {
            let Some(item) = course_item(course, &self.base_url) else {
                continue;
            };
            live_ids.insert(item.external_id().to_string());
            let course_id = item.external_id().trim_start_matches("course:").to_string();
            out.push(Ok(FetchEvent::Item(item)));

            let entries = match self.list_curriculum(http, &token, &course_id) {
                Ok(e) => e,
                Err(e) => {
                    // One inaccessible course must not lose the rest
                    // of the run — but reconciling against a walk
                    // that's missing its lectures would falsely sweep
                    // them, so the marker below is withheld.
                    eprintln!(
                        "udemy: curriculum for course {course_id} failed ({e}) — partial \
                         enumeration, deletion detection skipped this run"
                    );
                    partial = true;
                    continue;
                }
            };

            let mut chapter_title: Option<String> = None;
            for entry in &entries {
                if entry.get("_class").and_then(|v| v.as_str()) == Some("chapter") {
                    chapter_title = entry
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    continue;
                }
                let Some(mut lecture) =
                    curriculum_item(course, entry, chapter_title.as_deref(), &self.base_url)
                else {
                    continue;
                };
                if self.config.download_videos && is_video(entry) {
                    if let Some(local) = self.maybe_download_video(ctx, course, entry) {
                        lecture.media.push(MediaRef {
                            url: local.to_string_lossy().to_string(),
                            kind: "video".to_string(),
                            filename: local.file_name().map(|n| n.to_string_lossy().to_string()),
                            mime: None,
                            data: None,
                        });
                    }
                }
                live_ids.insert(lecture.external_id().to_string());
                out.push(Ok(FetchEvent::Item(lecture)));
            }

            done += 1;
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: serde_json::json!({"courses_done": done}),
                },
                note: format!("after course {course_id}"),
            })));
        }

        if !partial {
            out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                live_ids,
            ))));
        }

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(
        http: ManagedHttpClient,
        token: Option<&str>,
        cookies_file: Option<&str>,
        download_dir: Option<PathBuf>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("UDEMY_ACCESS_TOKEN".to_string(), t.to_string());
        }
        if let Some(c) = cookies_file {
            store.insert("UDEMY_COOKIES_FILE".to_string(), c.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "udemy".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(
                store,
                vec![
                    "UDEMY_ACCESS_TOKEN".to_string(),
                    "UDEMY_COOKIES_FILE".to_string(),
                ],
            ),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir,
            items_failed: 0,
            cancel: None,
            http: Some(RefCell::new(http)),
        }
    }

    fn no_sleep_client() -> ManagedHttpClient {
        ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), |_| {})
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-connector-udemy-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fake_yt_dlp(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn course_json(id: i64, title: &str, slug: &str) -> Value {
        serde_json::json!({
            "id": id,
            "title": title,
            "url": format!("/course/{slug}/"),
            "image_480x270": format!("https://img.udemycdn.com/{id}.jpg"),
            "published_title": slug,
        })
    }

    fn results_page(items: Vec<Value>, next: Option<&str>) -> Value {
        serde_json::json!({"results": items, "next": next})
    }

    fn chapter_json(title: &str) -> Value {
        serde_json::json!({"_class": "chapter", "title": title})
    }

    fn lecture_json(id: i64, title: &str, index: i64) -> Value {
        serde_json::json!({
            "_class": "lecture",
            "id": id,
            "title": title,
            "object_index": index,
            "asset": {"asset_type": "Video"},
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<Result<FetchEvent, ConnectorError>> {
        iter.collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = UdemyConnector::new(UdemyConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "udemy".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["UDEMY_ACCESS_TOKEN".to_string()]),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: None,
        };
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_without_a_token_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), None, None, None);
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn full_fetch_yields_course_lecture_and_quiz_items_and_a_reconcile_marker() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Rust 101", "rust-101")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(
            vec![
                chapter_json("Chapter One"),
                lecture_json(10, "Intro", 1),
                serde_json::json!({"_class": "quiz", "id": 11, "title": "Quiz One", "object_index": 2}),
            ],
            None,
        );
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/\?.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 3, "{evs:?}");
        let course = items.iter().find(|i| i.item_kind == "course").unwrap();
        assert_eq!(course.external_id(), "course:1");
        let lecture = items.iter().find(|i| i.item_kind == "lecture").unwrap();
        assert_eq!(lecture.external_id(), "lecture:1:10");
        assert_eq!(
            lecture.tags,
            vec!["Rust 101".to_string(), "Chapter One".to_string()]
        );
        let quiz = items.iter().find(|i| i.item_kind == "quiz").unwrap();
        // Quizzes share the "lecture:" identity prefix per the reference.
        assert_eq!(quiz.external_id(), "lecture:1:11");

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("course:1"));
        assert!(marker.live_ids.contains("lecture:1:10"));
        assert!(marker.live_ids.contains("lecture:1:11"));
    }

    #[test]
    fn course_filter_limits_to_matching_courses_by_id_or_slug() {
        let mut server = mockito::Server::new();
        let courses = results_page(
            vec![
                course_json(1, "Rust 101", "rust-101"),
                course_json(2, "Go 101", "go-101"),
                course_json(3, "Python 101", "python-101"),
            ],
            None,
        );
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let empty_curriculum = results_page(vec![], None);
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/.*/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(empty_curriculum.to_string())
            .create();

        let config = UdemyConfig {
            course_filter: vec!["1".to_string(), "python-101".to_string()],
            ..Default::default()
        };
        let mut connector = UdemyConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let course_ids: HashSet<String> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "course" => Some(i.external_id().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            course_ids,
            HashSet::from(["course:1".to_string(), "course:3".to_string()])
        );
    }

    #[test]
    fn a_failed_curriculum_withholds_the_reconcile_marker_but_keeps_other_courses() {
        let mut server = mockito::Server::new();
        let courses = results_page(
            vec![
                course_json(1, "Broken Course", "broken"),
                course_json(2, "Good Course", "good"),
            ],
            None,
        );
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let _m_curr_bad = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(500)
            .with_body("boom")
            .create();
        let curriculum_good = results_page(vec![lecture_json(20, "Good Lecture", 1)], None);
        let _m_curr_good = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/2/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum_good.to_string())
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let result = events(connector.fetch(&ctx));
        assert!(!result.iter().any(|r| r.is_err()), "{result:?}");
        let evs: Vec<_> = result.into_iter().map(|r| r.unwrap()).collect();
        assert!(
            !evs.iter()
                .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))),
            "{evs:?}"
        );
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 3, "{evs:?}"); // both courses + the good course's lecture
    }

    #[test]
    fn pagination_follows_next_links_for_courses() {
        let mut server = mockito::Server::new();
        let next_url = format!(
            "{}/api-2.0/users/me/subscribed-courses/?page=2",
            server.url()
        );
        let page0 = results_page(vec![course_json(1, "First", "first")], Some(&next_url));
        let page1 = results_page(vec![course_json(2, "Second", "second")], None);
        let _m0 = server
            .mock("GET", "/api-2.0/users/me/subscribed-courses/")
            .match_query(mockito::Matcher::Regex(r"page_size=".to_string()))
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock("GET", "/api-2.0/users/me/subscribed-courses/")
            .match_query(mockito::Matcher::Regex(r"page=2".to_string()))
            .with_status(200)
            .with_body(page1.to_string())
            .create();
        let empty_curriculum = results_page(vec![], None);
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/.*/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(empty_curriculum.to_string())
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let courses: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, FetchEvent::Item(i) if i.item_kind == "course"))
            .collect();
        assert_eq!(courses.len(), 2, "{evs:?}");
    }

    #[test]
    fn article_lecture_keeps_its_body_html() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Course", "course")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(
            vec![serde_json::json!({
                "_class": "lecture",
                "id": 30,
                "title": "An Article",
                "object_index": 1,
                "asset": {"asset_type": "Article", "body": "<p>hello</p>"},
            })],
            None,
        );
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let lecture = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "lecture" => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(lecture.body.as_deref(), Some("<p>hello</p>"));
    }

    #[test]
    fn supplementary_assets_become_file_media_refs() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Course", "course")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(
            vec![serde_json::json!({
                "_class": "lecture",
                "id": 40,
                "title": "With slides",
                "object_index": 1,
                "asset": {"asset_type": "Video"},
                "supplementary_assets": [{
                    "filename": "slides.pdf",
                    "download_urls": {"File": [{"file": "https://example.com/slides.pdf"}]},
                }],
            })],
            None,
        );
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let lecture = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "lecture" => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(lecture.media.len(), 1);
        assert_eq!(lecture.media[0].kind, "file");
        assert_eq!(lecture.media[0].url, "https://example.com/slides.pdf");
        assert_eq!(lecture.media[0].filename.as_deref(), Some("slides.pdf"));
    }

    #[test]
    fn download_videos_off_by_default_has_no_video_media() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Course", "course")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(vec![lecture_json(50, "A Video", 1)], None);
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let lecture = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "lecture" => Some(i),
                _ => None,
            })
            .unwrap();
        assert!(lecture.media.is_empty());
    }

    #[test]
    fn download_videos_downloads_via_yt_dlp_when_cookies_file_exists() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Course", "course")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(vec![lecture_json(60, "A Video", 1)], None);
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let dir = temp_dir("download");
        let fake = write_fake_yt_dlp(
            &dir,
            "fake-yt-dlp.sh",
            r#"prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then
    out=$(echo "$arg" | sed 's/%(ext)s/mp4/')
    printf 'fake-video-bytes' > "$out"
    echo "progress"
  fi
  prev="$arg"
done
exit 0"#,
        );
        let cookies = dir.join("cookies.txt");
        std::fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();

        let config = UdemyConfig {
            download_videos: true,
            download_timeout: 0, // watchdog disabled; runs inline
            ..Default::default()
        };
        let mut connector = UdemyConnector::new(config)
            .with_base_url(server.url())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(
            no_sleep_client(),
            Some("tok"),
            Some(&cookies.to_string_lossy()),
            Some(dir.clone()),
        );
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let lecture = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "lecture" => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(lecture.media.len(), 1);
        assert_eq!(lecture.media[0].kind, "video");
        let local_path = std::path::Path::new(&lecture.media[0].url);
        assert!(local_path.exists(), "{local_path:?} should exist");
        assert!(local_path.starts_with(&dir));
        assert_eq!(std::fs::read(local_path).unwrap(), b"fake-video-bytes");
    }

    #[test]
    fn download_videos_without_a_valid_cookies_file_is_skipped_not_fatal() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Course", "course")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(vec![lecture_json(70, "A Video", 1)], None);
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let dir = temp_dir("no-cookies");
        let config = UdemyConfig {
            download_videos: true,
            ..Default::default()
        };
        let mut connector = UdemyConnector::new(config).with_base_url(server.url());
        // UDEMY_COOKIES_FILE is declared but never set — Secrets::get
        // returns an Auth error, which the download path logs and
        // treats as best-effort, not fatal to the run.
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, Some(dir.clone()));
        let result = events(connector.fetch(&ctx));
        assert!(!result.iter().any(|r| r.is_err()), "{result:?}");
        let evs: Vec<_> = result.into_iter().map(|r| r.unwrap()).collect();
        let lecture = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "lecture" => Some(i),
                _ => None,
            })
            .unwrap();
        assert!(lecture.media.is_empty());
    }

    #[test]
    fn an_existing_download_is_reused_without_invoking_yt_dlp() {
        let mut server = mockito::Server::new();
        let courses = results_page(vec![course_json(1, "Course", "course")], None);
        let _m_courses = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/\?.*".to_string()),
            )
            .with_status(200)
            .with_body(courses.to_string())
            .create();
        let curriculum = results_page(vec![lecture_json(80, "Existing Video", 1)], None);
        let _m_curr = server
            .mock(
                "GET",
                mockito::Matcher::Regex(
                    r"^/api-2\.0/courses/1/subscriber-curriculum-items/.*".to_string(),
                ),
            )
            .with_status(200)
            .with_body(curriculum.to_string())
            .create();

        let dir = temp_dir("reuse");
        let fake = write_fake_yt_dlp(&dir, "must-not-run.sh", "exit 1");
        let cookies = dir.join("cookies.txt");
        std::fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();
        // Matches `maybe_download_video`'s naming: `{index:03} - {safe_name(title)}.<ext>`.
        let folder = dir.join("course");
        std::fs::create_dir_all(&folder).unwrap();
        let existing = folder.join("001 - Existing Video.mp4");
        std::fs::write(&existing, b"already-downloaded").unwrap();

        let config = UdemyConfig {
            download_videos: true,
            download_timeout: 0,
            ..Default::default()
        };
        let mut connector = UdemyConnector::new(config)
            .with_base_url(server.url())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(
            no_sleep_client(),
            Some("tok"),
            Some(&cookies.to_string_lossy()),
            Some(dir.clone()),
        );
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let lecture = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) if i.item_kind == "lecture" => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(lecture.media[0].url, existing.to_string_lossy());
        assert_eq!(std::fs::read(&existing).unwrap(), b"already-downloaded");
    }

    #[test]
    fn a_401_response_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/.*".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("bad"), None, None);
        let result = events(connector.fetch(&ctx));
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn a_non_auth_error_status_is_classified_as_transient() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api-2\.0/users/me/subscribed-courses/.*".to_string()),
            )
            .with_status(500)
            .with_body("{}")
            .create();

        let mut connector = UdemyConnector::new(UdemyConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None, None);
        let result = events(connector.fetch(&ctx));
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Transient(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn safe_name_collapses_runs_and_strips_edges() {
        assert_eq!(safe_name("Hello, World!!"), "Hello- World");
        assert_eq!(safe_name(""), "untitled");
        assert_eq!(safe_name("   "), "untitled");
        assert_eq!(safe_name("normal_name-1.2"), "normal_name-1.2");
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = UdemyConnector::new(UdemyConfig::default());
        assert_eq!(connector.type_name(), "udemy");
        assert_eq!(
            connector.secret_keys(),
            &[
                "UDEMY_ACCESS_TOKEN".to_string(),
                "UDEMY_COOKIES_FILE".to_string()
            ]
        );
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 3);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(connector.capabilities().supports_full_enumeration);
        assert!(connector.capabilities().produces_media);
        assert_eq!(connector.capabilities().concurrency, "serial");
        assert_eq!(
            connector.volatile_fields(),
            &[
                "completion_ratio".to_string(),
                "last_accessed_time".to_string()
            ]
        );
    }
}
