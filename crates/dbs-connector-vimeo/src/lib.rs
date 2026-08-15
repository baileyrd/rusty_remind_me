//! Vimeo connector: backs up the videos you own on your Vimeo account
//! (issue #94). Mirrors `dbs.connectors.vimeo` in
//! baileyrd/Daily-Backup-System.
//!
//! Vimeo has an official REST API (`https://api.vimeo.com`, version
//! 3.4). This connector reads your own library through
//! `GET /me/videos` using a **personal access token** (generated once
//! at developer.vimeo.com/apps). It stores the **catalog** — id,
//! title, link, dates, privacy, thumbnail — and keeps the verbatim
//! API object in `raw`; the watch URL rides along as a `MediaRef`.
//!
//! **Media, two levels.** By default no video bytes are downloaded
//! (the catalog is the backup, and direct file/download links via the
//! API require a *paid* Vimeo plan). Set `download_videos = true` to
//! additionally pull each video file with the `yt-dlp` binary into
//! the source's download folder — this works for your public videos
//! regardless of plan. Vimeo rejects a plain TLS fingerprint on
//! data-center/VPN IPs, so the download always passes
//! `--impersonate chrome` (yt-dlp's own `curl_cffi`-backed flag); if
//! that backend isn't installed, yt-dlp itself fails the download,
//! which is logged and retried on a later run — never fatal.
//!
//! Like `podcast`/`pocketcasts` this is a **full-enumeration**
//! source: the library is small and the API gives no server-side
//! `since` filter that also catches edits, so every run re-reads
//! `/me/videos` (`supports_incremental = false`) and a single
//! [`ReconcileMarker`][dbs_core::ReconcileMarker] lets the engine
//! soft-delete videos you've since removed. `stats`/`metadata` (play
//! counts, hypermedia links with short-lived tokens) churn every
//! response and are stripped via `volatile_fields` so an unchanged
//! video never spawns a spurious revision.
//!
//! Auth is the bearer token secret `VIMEO_TOKEN`.
//!
//! **Subprocess, not a library call.** Per this port's round-1
//! decision (see `dbs-connector-support`'s doc comment), `yt-dlp` is
//! shelled out to as a binary rather than called through a Python-
//! library-style API, so there's no `impersonate_target()` capability
//! probe to port — `--impersonate chrome` is passed unconditionally
//! and a failure surfaces as an ordinary (logged, non-fatal) download
//! failure. Stall detection uses
//! [`dbs_connector_support::run_with_watchdog`] with a heartbeat fed
//! by the subprocess's own stdout lines (`--newline` forces periodic
//! progress output) rather than a per-event progress-hook callback,
//! since the CLI has no equivalent hook — any stdout activity counts
//! as progress.
//!
//! Reachable from a real `dbs backup vimeo` run since #164's
//! `dbs-connector-vimeo` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses; the download path
//! is tested against a fake `yt-dlp` script on disk, the same pattern
//! `dbs-research`'s YouTube search already uses.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbs_connector_support::{run_with_watchdog, WatchdogError};
use dbs_core::export_profile::ExportProfile;
use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, MediaRef, RunContext,
};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.vimeo.com";
// Pin the API version so a server-side default bump can't silently
// reshape the payload (Vimeo's own guidance: always request an
// explicit version).
const API_VERSION: &str = "application/vnd.vimeo.*+json;version=3.4";

#[derive(Debug, Clone)]
pub struct VimeoConfig {
    pub token_env: String,
    /// Videos requested per API page (Vimeo max is 100).
    pub page_size: u32,
    /// Off by default: the catalog IS the backup, and API download
    /// links need a paid plan. On, video bytes are pulled with
    /// `yt-dlp` (works for your public videos on any plan) into the
    /// source's download folder.
    pub download_videos: bool,
    /// Where downloaded video files land. `None` defaults to the
    /// engine's per-source folder (`ctx.download_dir`).
    pub downloads_dir: Option<String>,
    /// Cap the selected variant's height (e.g. 1080, 720). 0 = best
    /// available.
    pub video_quality: u32,
    /// Abandon a video download after this many seconds without
    /// progress; retried on a later run. 0 = no watchdog.
    pub video_stall_timeout: u64,
}

impl Default for VimeoConfig {
    fn default() -> Self {
        Self {
            token_env: "VIMEO_TOKEN".to_string(),
            page_size: 100,
            download_videos: false,
            downloads_dir: None,
            video_quality: 1080,
            video_stall_timeout: 180,
        }
    }
}

pub struct VimeoConnector {
    config: VimeoConfig,
    base_url: String,
    yt_dlp_bin: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl VimeoConnector {
    pub fn new(config: VimeoConfig) -> Self {
        let token_env = config.token_env.clone();
        Self {
            config,
            base_url: DEFAULT_BASE_URL.to_string(),
            yt_dlp_bin: "yt-dlp".to_string(),
            secret_keys: vec![token_env],
            item_kinds: vec![ItemKind {
                name: "video".to_string(),
                display_name: "Video".to_string(),
                description: String::new(),
            }],
            // Play counts and the hypermedia `metadata` block change
            // on every fetch; strip before hashing so an otherwise-
            // unchanged video never spawns a revision for them alone.
            volatile_fields: vec!["stats".to_string(), "metadata".to_string()],
        }
    }

    /// Overrides the API base URL (default `https://api.vimeo.com`)
    /// — for tests to point at a local mock server.
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

    fn get_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        page: u32,
    ) -> Result<Value, ConnectorError> {
        let url = format!("{}/me/videos", self.base_url);
        let params = [
            ("per_page", self.config.page_size.to_string()),
            ("page", page.to_string()),
            ("sort", "date".to_string()),
            ("direction", "desc".to_string()),
        ];
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, &url, |b| {
                b.bearer_auth(token)
                    .header("Accept", API_VERSION)
                    .query(&params)
            })
            .map_err(classify_api_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    /// Where files land: an explicit `downloads_dir` wins, else the
    /// engine-provided per-source folder.
    fn downloads_root(&self, ctx: &RunContext) -> Result<PathBuf, ConnectorError> {
        if let Some(d) = &self.config.downloads_dir {
            return Ok(PathBuf::from(d));
        }
        ctx.download_dir.clone().ok_or_else(|| {
            ConnectorError::Config(
                "no download folder: set downloads_dir on the vimeo source or download_root in \
                 [dbs]."
                    .to_string(),
            )
        })
    }

    /// Best-effort: download one video's file into `downloads` and,
    /// on success, tag `raw["_video_path"]` so [`to_item`] maps it to
    /// a local `MediaRef` (in place of the watch-link reference). A
    /// failure is logged and retried next run — it never fails the
    /// backup or the other videos.
    fn maybe_download(&self, downloads: &Path, vid: &str, raw: &mut Value) {
        let Some(link) = raw
            .get("link")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let name = raw.get("name").and_then(|v| v.as_str());
        let dest = downloads.join(format!("{vid}{}.mp4", safe_suffix(name)));
        if dest.exists() && dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            set_video_path(raw, &dest);
            return;
        }
        let ok = download_video(
            &self.yt_dlp_bin,
            &link,
            &dest,
            self.config.video_quality,
            self.config.video_stall_timeout,
        );
        if ok && dest.exists() && dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            eprintln!("vimeo: downloaded {vid} -> {}", dest.display());
            set_video_path(raw, &dest);
        }
    }
}

fn set_video_path(raw: &mut Value, dest: &Path) {
    if let Some(obj) = raw.as_object_mut() {
        obj.insert(
            "_video_path".to_string(),
            Value::String(dest.to_string_lossy().to_string()),
        );
    }
}

/// Numeric id from a Vimeo `uri` (`/videos/12345` -> `"12345"`).
/// Returns `None` for a malformed/idless uri (e.g. `/videos/`) rather
/// than a bogus segment.
fn video_id(uri: Option<&str>) -> Option<String> {
    let uri = uri?;
    let parts: Vec<&str> = uri.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 && parts[parts.len() - 2] == "videos" {
        let last = parts[parts.len() - 1].trim();
        if !last.is_empty() {
            return Some(last.to_string());
        }
    }
    None
}

fn is_unsafe_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
}

/// A `` - <safe title>`` filename suffix, or `""` when there's no
/// usable title. Only characters Windows forbids are stripped, so
/// readable titles survive; the id already guarantees a unique, valid
/// base name.
fn safe_suffix(name: Option<&str>) -> String {
    let cleaned: String = name
        .unwrap_or("")
        .chars()
        .map(|c| if is_unsafe_char(c) { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c| c == '.' || c == '_' || c == ' ');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(" - {}", trimmed.chars().take(120).collect::<String>())
    }
}

fn to_item(raw: &Value) -> Option<BackupItem> {
    let vid = video_id(raw.get("uri").and_then(|v| v.as_str()))?;
    let mut media = Vec::new();
    if let Some(thumb) = raw
        .get("pictures")
        .and_then(|p| p.get("base_link"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        media.push(MediaRef {
            url: thumb.to_string(),
            kind: "image".to_string(),
            filename: None,
            mime: None,
            data: None,
        });
    }
    let video_path = raw.get("_video_path").and_then(|v| v.as_str());
    let link = raw
        .get("link")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    // A downloaded file (local path) wins; otherwise keep the watch
    // link as the reference of record.
    if let Some(vp) = video_path {
        media.push(MediaRef {
            url: vp.to_string(),
            kind: "video".to_string(),
            filename: Path::new(vp)
                .file_name()
                .map(|n| n.to_string_lossy().to_string()),
            mime: None,
            data: None,
        });
    } else if let Some(l) = link {
        media.push(MediaRef {
            url: l.to_string(),
            kind: "video".to_string(),
            filename: None,
            mime: None,
            data: None,
        });
    }
    let tags = raw
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut item = BackupItem::new(vid, "video", raw.clone()).ok()?;
    item.title = raw
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.url = link.map(str::to_string);
    item.body = raw
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.tags = tags;
    item.created_at = raw
        .get("created_time")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item.updated_at = raw
        .get("modified_time")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item.media = media;
    Some(item)
}

/// Downloads `url` to `dest` via the `yt-dlp` binary. Returns `true`
/// on success. A stall watchdog abandons the subprocess (never
/// force-killed — same constraint as the reference's threads) if no
/// stdout activity arrives within `stall_timeout_secs`.
fn download_video(
    yt_dlp_bin: &str,
    url: &str,
    dest: &Path,
    quality: u32,
    stall_timeout_secs: u64,
) -> bool {
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "vimeo: video download failed ({}): {e}",
                dest.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
            );
            return false;
        }
    }
    let dest_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut cmd = Command::new(yt_dlp_bin);
    cmd.arg("--quiet")
        .arg("--no-warnings")
        .arg("--newline") // periodic stdout progress lines feed the stall heartbeat
        .arg("-o")
        .arg(dest)
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("--concurrent-fragments")
        .arg("8")
        .arg("--socket-timeout")
        .arg("30")
        .arg("--retries")
        .arg("10")
        .arg("--fragment-retries")
        .arg("10");
    if quality > 0 {
        cmd.arg("--format-sort")
            .arg(format!("res:{quality},vcodec:h264,acodec:m4a"));
    }
    // Vimeo blocks yt-dlp's default TLS fingerprint on data-center/VPN
    // IPs; always impersonate a real browser. If the curl_cffi backend
    // isn't installed, yt-dlp itself fails the download — caught below
    // like any other failure.
    cmd.arg("--impersonate").arg("chrome");
    cmd.arg(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vimeo: video download failed ({dest_name}): {e}");
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
        &format!("vimeo video download {dest_name}"),
        heartbeat_ref,
    );
    if let Some(h) = reader_handle {
        let _ = h.join();
    }

    match result {
        Ok(status) => status.success(),
        Err(WatchdogError::Timeout(t)) => {
            eprintln!("vimeo: video download stalled ({dest_name}): {t}");
            false
        }
        Err(WatchdogError::Inner(e)) => {
            eprintln!("vimeo: video download failed ({dest_name}): {e}");
            false
        }
        Err(WatchdogError::WorkerPanicked) => {
            eprintln!("vimeo: video download failed ({dest_name}): worker thread panicked");
            false
        }
    }
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). Vimeo rejects a bad token with 401 or 403; everything
/// else non-retryable is a transient upstream problem.
fn classify_api_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, .. } => match error.status().map(|s| s.as_u16()) {
            Some(status @ (401 | 403)) => {
                ConnectorError::Auth(format!("Vimeo rejected the token ({status})"))
            }
            Some(status) => ConnectorError::Transient(format!("Vimeo API error {status}")),
            None => ConnectorError::Transient(error.to_string()),
        },
    }
}

fn bool_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<bool>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ConnectorError::Config(format!("sources.<name>.{key} must be a bool, got {v}"))
    })
}

fn string_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<String>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_str().map(str::to_string).map(Some).ok_or_else(|| {
        ConnectorError::Config(format!("sources.<name>.{key} must be a string, got {v}"))
    })
}

fn u32_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<u32>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    let n = v.as_u64().ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be a non-negative integer, got {v}"
        ))
    })?;
    u32::try_from(n)
        .map(Some)
        .map_err(|_| ConnectorError::Config(format!("sources.<name>.{key} is too large, got {n}")))
}

fn u64_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<u64>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_u64().map(Some).ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be a non-negative integer, got {v}"
        ))
    })
}

fn ranged_u32_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
    min: u32,
    max: u32,
) -> Result<Option<u32>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    let n = v.as_u64().ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be a positive integer, got {v}"
        ))
    })?;
    if n < min as u64 || n > max as u64 {
        return Err(ConnectorError::Config(format!(
            "sources.<name>.{key} must be between {min} and {max}, got {n}"
        )));
    }
    Ok(Some(n as u32))
}

impl Connector for VimeoConnector {
    fn type_name(&self) -> &str {
        "vimeo"
    }

    fn display_name(&self) -> &str {
        "Vimeo"
    }

    fn description(&self) -> &str {
        "Videos you own on Vimeo, via the official REST API (v3.4)."
    }

    fn docs_url(&self) -> &str {
        "https://developer.vimeo.com/api/reference/videos"
    }

    fn setup_hint(&self) -> &str {
        "Generate a personal access token at developer.vimeo.com/apps, then set VIMEO_TOKEN in \
         your .env. download_videos is off by default (catalog only)."
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

    fn export_profile(&self) -> Option<ExportProfile> {
        Some(ExportProfile {
            group_by: vec!["user_name".to_string()],
            body_from: vec!["description".to_string()],
            ..Default::default()
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: false,     // re-read /me/videos every run
            supports_full_enumeration: true, // enables the soft-delete reconcile sweep
            supports_native_deletes: false,  // removals detected via reconcile only
            produces_media: true,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: true,
            concurrency: "serial".to_string(), // the opt-in download path drives yt-dlp
            ..Capabilities::default()
        }
    }

    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = ranged_u32_option(options, "page_size", 1, 100)? {
            self.config.page_size = v;
        }
        if let Some(v) = bool_option(options, "download_videos")? {
            self.config.download_videos = v;
        }
        if let Some(v) = string_option(options, "downloads_dir")? {
            self.config.downloads_dir = Some(v);
        }
        if let Some(v) = u32_option(options, "video_quality")? {
            self.config.video_quality = v;
        }
        if let Some(v) = u64_option(options, "video_stall_timeout")? {
            self.config.video_stall_timeout = v;
        }
        Ok(())
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        let Some(http) = ctx.http.as_ref() else {
            out.push(Err(ConnectorError::Config(
                "Vimeo connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        if !self.secret_keys.contains(&self.config.token_env) {
            out.push(Err(ConnectorError::Config(format!(
                "token_env={:?} must be one of the declared secret_keys {:?}; set VIMEO_TOKEN in \
                 your .env.",
                self.config.token_env, self.secret_keys
            ))));
            return Box::new(out.into_iter());
        }
        let token = match ctx.secrets.get(&self.config.token_env) {
            Ok(t) => t.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let downloads = if self.config.download_videos {
            match self.downloads_root(ctx) {
                Ok(d) => Some(d),
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            }
        } else {
            None
        };

        let mut live_ids = HashSet::new();
        let mut seen = 0u32;
        let mut page = 1u32;
        loop {
            let data = match self.get_page(http, &token, page) {
                Ok(d) => d,
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };
            let entries = data
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if entries.is_empty() {
                break;
            }
            for raw in &entries {
                let Some(vid) = video_id(raw.get("uri").and_then(|v| v.as_str())) else {
                    continue;
                };
                // Download BEFORE mapping so the on-disk path is baked
                // into the item's media (raw is copied into
                // BackupItem.raw, so a later mutation wouldn't reach
                // the stored item).
                let mut raw = raw.clone();
                if let Some(downloads) = &downloads {
                    self.maybe_download(downloads, &vid, &mut raw);
                }
                let Some(item) = to_item(&raw) else {
                    continue;
                };
                live_ids.insert(item.external_id().to_string());
                out.push(Ok(FetchEvent::Item(item)));
                seen += 1;
            }
            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                cursor: Cursor {
                    value: serde_json::json!({"videos_seen": seen}),
                },
                note: format!("page {page}"),
            })));
            // Stop when Vimeo reports no next page, or the page came
            // back short.
            let has_next = data
                .get("paging")
                .and_then(|p| p.get("next"))
                .is_some_and(|v| !v.is_null());
            if !has_next || (entries.len() as u32) < self.config.page_size {
                break;
            }
            page += 1;
        }

        out.push(Ok(FetchEvent::ReconcileMarker(
            dbs_core::ReconcileMarker::new(live_ids),
        )));

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
        download_dir: Option<PathBuf>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("VIMEO_TOKEN".to_string(), t.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "vimeo".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(store, vec!["VIMEO_TOKEN".to_string()]),
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
            "dbs-connector-vimeo-test-{label}-{}-{:?}",
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

    fn video_json(id: &str, name: &str, link: &str) -> Value {
        serde_json::json!({
            "uri": format!("/videos/{id}"),
            "name": name,
            "link": link,
            "description": "a video",
            "created_time": "2024-06-01T00:00:00Z",
            "modified_time": "2024-06-02T00:00:00Z",
            "pictures": {"base_link": format!("https://i.vimeocdn.com/{id}.jpg")},
            "tags": [{"name": "rust"}],
        })
    }

    fn page(items: Vec<Value>, next: Option<&str>) -> Value {
        serde_json::json!({"data": items, "paging": {"next": next}})
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<Result<FetchEvent, ConnectorError>> {
        iter.collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = VimeoConnector::new(VimeoConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "vimeo".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["VIMEO_TOKEN".to_string()]),
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
        let mut connector = VimeoConnector::new(VimeoConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), None, None);
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn full_fetch_yields_videos_and_a_reconcile_marker() {
        let mut server = mockito::Server::new();
        let body = page(
            vec![video_json("1", "A Video", "https://vimeo.com/1")],
            None,
        );
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/me/videos\?.*".to_string()),
            )
            .with_status(200)
            .with_body(body.to_string())
            .create();

        let mut connector = VimeoConnector::new(VimeoConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None);
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
        assert_eq!(items.len(), 1, "{evs:?}");
        assert_eq!(items[0].external_id(), "1");
        assert_eq!(items[0].title.as_deref(), Some("A Video"));
        assert_eq!(items[0].media.len(), 2);
        assert_eq!(items[0].media[1].url, "https://vimeo.com/1");

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        assert!(marker.unwrap().live_ids.contains("1"));
    }

    #[test]
    fn pagination_follows_paging_next_across_pages() {
        let mut server = mockito::Server::new();
        let page0 = page(
            vec![video_json("1", "First", "https://vimeo.com/1")],
            Some("/me/videos?page=2"),
        );
        let page1 = page(vec![video_json("2", "Second", "https://vimeo.com/2")], None);
        let _m0 = server
            .mock("GET", "/me/videos")
            .match_query(mockito::Matcher::Regex(r"page=1".to_string()))
            .with_status(200)
            .with_body(page0.to_string())
            .create();
        let _m1 = server
            .mock("GET", "/me/videos")
            .match_query(mockito::Matcher::Regex(r"page=2".to_string()))
            .with_status(200)
            .with_body(page1.to_string())
            .create();

        let config = VimeoConfig {
            page_size: 1,
            ..Default::default()
        };
        let mut connector = VimeoConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None);
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
        assert_eq!(items.len(), 2, "{evs:?}");
    }

    #[test]
    fn a_video_with_no_numeric_id_in_the_uri_is_skipped() {
        let mut server = mockito::Server::new();
        let body = page(
            vec![serde_json::json!({"uri": "/videos/", "name": "bad"})],
            None,
        );
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/videos.*".to_string()))
            .with_status(200)
            .with_body(body.to_string())
            .create();

        let mut connector = VimeoConnector::new(VimeoConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None);
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter().any(|r| matches!(r, Ok(FetchEvent::Item(_)))),
            "{evs:?}"
        );
    }

    #[test]
    fn download_videos_off_by_default_uses_the_watch_link_as_media() {
        let mut server = mockito::Server::new();
        let body = page(
            vec![video_json("1", "A Video", "https://vimeo.com/1")],
            None,
        );
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/videos.*".to_string()))
            .with_status(200)
            .with_body(body.to_string())
            .create();

        let mut connector = VimeoConnector::new(VimeoConfig::default()).with_base_url(server.url());
        assert!(!connector.config.download_videos);
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.media[1].kind, "video");
        assert_eq!(item.media[1].url, "https://vimeo.com/1");
    }

    #[test]
    fn download_videos_downloads_via_yt_dlp_and_prefers_the_local_path() {
        let mut server = mockito::Server::new();
        let body = page(
            vec![video_json("1", "A Video", "https://vimeo.com/1")],
            None,
        );
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/videos.*".to_string()))
            .with_status(200)
            .with_body(body.to_string())
            .create();

        let dir = temp_dir("download");
        // Writes bytes to whatever path follows `-o`, ignoring the rest.
        let fake = write_fake_yt_dlp(
            &dir,
            "fake-yt-dlp.sh",
            r#"prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then
    printf 'fake-video-bytes' > "$arg"
    echo "progress"
  fi
  prev="$arg"
done
exit 0"#,
        );

        let config = VimeoConfig {
            download_videos: true,
            video_stall_timeout: 0, // watchdog disabled; runs inline
            ..Default::default()
        };
        let mut connector = VimeoConnector::new(config)
            .with_base_url(server.url())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), Some(dir.clone()));
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.media[1].kind, "video");
        let local_path = std::path::Path::new(&item.media[1].url);
        assert!(local_path.exists(), "{local_path:?} should exist");
        assert!(local_path.starts_with(&dir));
        assert_eq!(std::fs::read(local_path).unwrap(), b"fake-video-bytes");
    }

    #[test]
    fn download_videos_with_no_download_dir_is_a_config_error() {
        let server = mockito::Server::new();
        let config = VimeoConfig {
            download_videos: true,
            ..Default::default()
        };
        let mut connector = VimeoConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None);
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn an_existing_nonempty_download_is_reused_without_invoking_yt_dlp() {
        let mut server = mockito::Server::new();
        let body = page(
            vec![video_json("1", "A Video", "https://vimeo.com/1")],
            None,
        );
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/videos.*".to_string()))
            .with_status(200)
            .with_body(body.to_string())
            .create();

        let dir = temp_dir("reuse");
        // A yt-dlp that would fail loudly if ever invoked.
        let fake = write_fake_yt_dlp(&dir, "must-not-run.sh", "exit 1");
        // Matches `maybe_download`'s naming: `{vid}{safe_suffix(name)}.mp4`.
        let existing = dir.join("1 - A Video.mp4");
        std::fs::write(&existing, b"already-downloaded").unwrap();

        let config = VimeoConfig {
            download_videos: true,
            video_stall_timeout: 0,
            ..Default::default()
        };
        let mut connector = VimeoConnector::new(config)
            .with_base_url(server.url())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), Some(dir.clone()));
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let item = evs
            .iter()
            .find_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(item.media[1].url, existing.to_string_lossy());
        assert_eq!(std::fs::read(&existing).unwrap(), b"already-downloaded");
    }

    #[test]
    fn a_401_response_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", mockito::Matcher::Regex(r"^/me/videos.*".to_string()))
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector = VimeoConnector::new(VimeoConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("bad"), None);
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
            .mock("GET", mockito::Matcher::Regex(r"^/me/videos.*".to_string()))
            .with_status(500)
            .with_body("{}")
            .create();

        let mut connector = VimeoConnector::new(VimeoConfig::default()).with_base_url(server.url());
        let ctx = ctx_with(no_sleep_client(), Some("tok"), None);
        let result = events(connector.fetch(&ctx));
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Transient(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn video_id_parses_a_valid_uri_and_rejects_a_malformed_one() {
        assert_eq!(video_id(Some("/videos/12345")), Some("12345".to_string()));
        assert_eq!(video_id(Some("/videos/")), None);
        assert_eq!(video_id(None), None);
        assert_eq!(video_id(Some("/users/1")), None);
    }

    #[test]
    fn safe_suffix_strips_unsafe_characters_and_truncates() {
        assert_eq!(safe_suffix(Some("A: Video?")), " - A Video");
        assert_eq!(safe_suffix(None), "");
        assert_eq!(safe_suffix(Some("   ")), "");
        let long = "x".repeat(200);
        assert_eq!(safe_suffix(Some(&long)).chars().count(), 3 + 120);
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = VimeoConnector::new(VimeoConfig::default());
        assert_eq!(connector.type_name(), "vimeo");
        assert_eq!(connector.secret_keys(), &["VIMEO_TOKEN".to_string()]);
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 1);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(connector.capabilities().supports_full_enumeration);
        assert!(connector.capabilities().produces_media);
        assert_eq!(connector.capabilities().concurrency, "serial");
        assert_eq!(
            connector.volatile_fields(),
            &["stats".to_string(), "metadata".to_string()]
        );
        assert!(connector.export_profile().is_some());
    }

    #[test]
    fn configure_applies_every_field_from_options() {
        let mut connector = VimeoConnector::new(VimeoConfig::default());
        let options = HashMap::from([
            ("page_size".to_string(), serde_json::json!(25)),
            ("download_videos".to_string(), serde_json::json!(true)),
            (
                "downloads_dir".to_string(),
                serde_json::json!("/tmp/vimeo-videos"),
            ),
            ("video_quality".to_string(), serde_json::json!(720)),
            ("video_stall_timeout".to_string(), serde_json::json!(60)),
        ]);
        connector.configure(&options).unwrap();
        assert_eq!(connector.config.page_size, 25);
        assert!(connector.config.download_videos);
        assert_eq!(
            connector.config.downloads_dir,
            Some("/tmp/vimeo-videos".to_string())
        );
        assert_eq!(connector.config.video_quality, 720);
        assert_eq!(connector.config.video_stall_timeout, 60);
    }

    #[test]
    fn configure_with_no_matching_keys_leaves_defaults_untouched() {
        let mut connector = VimeoConnector::new(VimeoConfig::default());
        let defaults = VimeoConfig::default();
        connector.configure(&HashMap::new()).unwrap();
        assert_eq!(connector.config.page_size, defaults.page_size);
        assert_eq!(connector.config.download_videos, defaults.download_videos);
        assert_eq!(connector.config.downloads_dir, defaults.downloads_dir);
        assert_eq!(connector.config.video_quality, defaults.video_quality);
        assert_eq!(
            connector.config.video_stall_timeout,
            defaults.video_stall_timeout
        );
    }

    #[test]
    fn configure_rejects_a_page_size_outside_1_to_100() {
        let mut connector = VimeoConnector::new(VimeoConfig::default());
        let options = HashMap::from([("page_size".to_string(), serde_json::json!(101))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_non_bool_download_videos() {
        let mut connector = VimeoConnector::new(VimeoConfig::default());
        let options = HashMap::from([("download_videos".to_string(), serde_json::json!("yes"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_negative_video_quality() {
        let mut connector = VimeoConnector::new(VimeoConfig::default());
        let options = HashMap::from([("video_quality".to_string(), serde_json::json!(-1))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }
}
