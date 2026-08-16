//! YouTube connector: backs up your account's lists (Watch Later,
//! Liked, ...) (issue #98). Mirrors `dbs.connectors.youtube` in
//! baileyrd/Daily-Backup-System.
//!
//! YouTube exposes your private lists (watch history, the `WL` Watch
//! Later playlist, the `LL` Liked playlist, and every playlist you
//! own) only to your logged-in session. This connector shells out to
//! the `yt-dlp` binary with your cookies to do a *flat* extraction of
//! each list — fast metadata only, no media download. Only the
//! catalog (id, title, url, channel, duration) is stored, with the
//! video URL kept as a `MediaRef`.
//!
//! **Subprocess, not a library call**, same pattern as `vimeo` (#94)
//! and `udemy` (#95): `yt-dlp --dump-single-json --flat-playlist`
//! mirrors the reference's `yt_dlp.YoutubeDL(...).extract_info(url,
//! download=False)` call exactly — one JSON object with a top-level
//! `title` and an `entries` array. There's no per-item progress to
//! stream during a flat extraction (unlike a real video download), so
//! [`dbs_connector_support::run_with_watchdog`] here is a plain
//! wall-clock deadline on the whole extraction (`heartbeat: None`),
//! matching the reference's own `run_with_watchdog(..., timeout=...)`
//! call, which likewise passes no heartbeat.
//!
//! Like `reddit`/`skool` this is a **full-enumeration** source: no
//! server-side `since` filter, so every run is full and a single
//! [`ReconcileMarker`][dbs_core::ReconcileMarker] lets the engine
//! soft-delete anything removed from a list — unless a list failed to
//! load, in which case the whole run's marker is withheld (the same
//! "one bad group taints the sweep" shape as `skool`'s per-community
//! partial enumeration). A video can live in several lists at once,
//! so `external_id` is namespaced by list (`"<list>:<video_id>"`) —
//! the same video in Watch Later and Liked stays two distinct,
//! independently-tracked items; the *same* video listed twice
//! *within* one list keeps only its first occurrence.
//!
//! Auth is a **path-valued secret**, `YOUTUBE_COOKIES_FILE` (a
//! Netscape cookies.txt), or `cookies_from_browser` in config to read
//! cookies straight from a local browser profile instead (no secret
//! needed).
//!
//! Unlike `reddit`/`skool`/`vimeo`/`udemy`, **acquisition here is not
//! blocked** — no Playwright browser is needed at all
//! (`wants_managed_http = false`, matching the reference), so this
//! connector's `fetch()` is fully implemented and exercised end to
//! end against a fake `yt-dlp` script on disk, the same pattern
//! `dbs-connector-vimeo`/`dbs-connector-udemy` already use.

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::time::Duration;

use dbs_connector_support::{run_with_watchdog, WatchdogError};
use dbs_core::export_profile::ExportProfile;
use dbs_core::{
    AuthCapture, BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor,
    FetchEvent, ItemKind, MediaRef, ReconcileMarker, RunContext,
};
use serde_json::Value;

const WATCH_LATER: (&str, &str) = ("watch-later", "https://www.youtube.com/playlist?list=WL");
const LIKED: (&str, &str) = ("liked", "https://www.youtube.com/playlist?list=LL");
const HISTORY: (&str, &str) = ("watch-history", ":ythistory");

const ENTRY_FIELDS: &[&str] = &[
    "id",
    "title",
    "url",
    "duration",
    "channel",
    "channel_id",
    "uploader",
    "view_count",
    "live_status",
];

#[derive(Debug, Clone)]
pub struct YouTubeConfig {
    pub watch_later: bool,
    pub liked: bool,
    /// Huge and timestamp-less via this route; opt-in.
    pub history: bool,
    pub playlists: bool,
    pub max_history: u32,
    /// Abandon a list extraction after this many seconds; the run
    /// continues with the list marked failed. 0 = no cap.
    pub extract_timeout: u64,
    pub cookies_file_env: String,
    pub cookies_from_browser: Option<String>,
}

impl Default for YouTubeConfig {
    fn default() -> Self {
        Self {
            watch_later: true,
            liked: true,
            history: false,
            playlists: true,
            max_history: 5000,
            extract_timeout: 600,
            cookies_file_env: "YOUTUBE_COOKIES_FILE".to_string(),
            cookies_from_browser: None,
        }
    }
}

pub struct YouTubeConnector {
    config: YouTubeConfig,
    yt_dlp_bin: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
    pip_requirements: Vec<String>,
    runtime_imports: Vec<String>,
    auth_capture: AuthCapture,
}

impl YouTubeConnector {
    pub fn new(config: YouTubeConfig) -> Self {
        Self {
            config,
            yt_dlp_bin: "yt-dlp".to_string(),
            secret_keys: vec!["YOUTUBE_COOKIES_FILE".to_string()],
            item_kinds: vec![ItemKind {
                name: "video".to_string(),
                display_name: "Video".to_string(),
                description: String::new(),
            }],
            // The capture timestamp churns every run, and view_count
            // drifts constantly; strip both before hashing so a video
            // never spawns revisions for them alone.
            volatile_fields: vec!["captured_at".to_string(), "view_count".to_string()],
            pip_requirements: vec![
                "yt-dlp[default]>=2026.1.29".to_string(),
                "nodejs-wheel>=22".to_string(),
            ],
            runtime_imports: vec!["yt_dlp".to_string()],
            auth_capture: AuthCapture {
                kind: "browser_cookies".to_string(),
                secret_key: "YOUTUBE_COOKIES_FILE".to_string(),
                login_url: "https://www.youtube.com/".to_string(),
                label: "YouTube login".to_string(),
                target_dir_option: String::new(),
                target_path: String::new(),
                per_source: true,
            },
        }
    }

    /// Overrides the `yt-dlp` binary name/path (default `"yt-dlp"`)
    /// — for tests to point at a fake script on disk.
    pub fn with_yt_dlp_bin(mut self, bin: impl Into<String>) -> Self {
        self.yt_dlp_bin = bin.into();
        self
    }

    /// Runs `yt-dlp --dump-single-json` against `source_url` and
    /// parses the resulting single JSON object. A wall-clock deadline
    /// (no heartbeat — there's no per-item progress to stream during
    /// a flat extraction) via `run_with_watchdog`, matching the
    /// reference's own timeout-only `run_with_watchdog` call.
    fn run_ytdlp_json(
        &self,
        description: &str,
        source_url: &str,
        playlist_end: Option<u32>,
        cookiefile: Option<&str>,
    ) -> Result<Value, String> {
        let mut cmd = Command::new(&self.yt_dlp_bin);
        cmd.arg("--quiet")
            .arg("--no-warnings")
            .arg("--skip-download")
            .arg("--flat-playlist")
            .arg("--ignore-errors") // deleted/private videos inside lists
            .arg("--socket-timeout")
            .arg("30")
            .arg("--dump-single-json");
        if let Some(end) = playlist_end {
            cmd.arg("--playlist-end").arg(end.to_string());
        }
        if let Some(cf) = cookiefile {
            cmd.arg("--cookies").arg(cf);
        }
        if let Some(cb) = &self.config.cookies_from_browser {
            cmd.arg("--cookies-from-browser").arg(cb);
        }
        cmd.arg(source_url);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd
            .spawn()
            .map_err(|e| format!("failed to launch yt-dlp: {e}"))?;
        let timeout = Duration::from_secs(self.config.extract_timeout);
        let result =
            run_with_watchdog(move || child.wait_with_output(), timeout, description, None);
        let output = match result {
            Ok(o) => o,
            Err(WatchdogError::Timeout(t)) => return Err(t.to_string()),
            Err(WatchdogError::Inner(e)) => return Err(e.to_string()),
            Err(WatchdogError::WorkerPanicked) => return Err("worker thread panicked".to_string()),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "yt-dlp exited with {}: {}",
                output.status,
                stderr.trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(stdout.trim()).map_err(|e| format!("invalid JSON output: {e}"))
    }

    /// One list's (title, non-null entries).
    fn dump_list(
        &self,
        label: &str,
        source_url: &str,
        playlist_end: Option<u32>,
        cookiefile: Option<&str>,
    ) -> Result<(Option<String>, Vec<Value>), String> {
        let info = self.run_ytdlp_json(
            &format!("youtube list {label}"),
            source_url,
            playlist_end,
            cookiefile,
        )?;
        let entries = info
            .get("entries")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter(|e| !e.is_null()).cloned().collect())
            .unwrap_or_default();
        let title = info
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok((title, entries))
    }

    /// `(label, url)` for each playlist the account owns. `None`
    /// means discovery itself *failed* (as opposed to "the account
    /// has no playlists") — the caller must treat the run as a
    /// partial enumeration.
    fn discover_playlists(&self, cookiefile: Option<&str>) -> Option<Vec<(String, String)>> {
        let info = match self.run_ytdlp_json(
            "youtube playlist discovery",
            "https://www.youtube.com/feed/playlists",
            None,
            cookiefile,
        ) {
            Ok(i) => i,
            Err(msg) => {
                eprintln!("youtube: could not list playlists: {msg}");
                return None;
            }
        };
        let entries = info
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for e in &entries {
            if e.is_null() {
                continue;
            }
            let pid = e.get("id").and_then(|v| v.as_str());
            let url = e
                .get("url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| pid.map(|p| format!("https://www.youtube.com/playlist?list={p}")));
            if let Some(u) = url {
                let title = e
                    .get("title")
                    .and_then(|v| v.as_str())
                    .or(pid)
                    .unwrap_or("");
                out.push((format!("playlist:{title}"), u));
            }
        }
        Some(out)
    }
}

fn entry_record(
    position: u32,
    e: &Value,
    list_label: &str,
    list_title: Option<&str>,
    captured_at: &str,
) -> Value {
    let mut rec = serde_json::Map::new();
    rec.insert("position".to_string(), Value::Number(position.into()));
    for f in ENTRY_FIELDS {
        let v = e.get(*f).cloned().unwrap_or(Value::Null);
        let key = if *f == "duration" {
            "duration_seconds"
        } else {
            f
        };
        rec.insert(key.to_string(), v);
    }
    let has_url = rec
        .get("url")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    if !has_url {
        if let Some(id) = rec
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            rec.insert(
                "url".to_string(),
                Value::String(format!("https://www.youtube.com/watch?v={id}")),
            );
        }
    }
    rec.insert(
        "list_label".to_string(),
        Value::String(list_label.to_string()),
    );
    rec.insert(
        "list_title".to_string(),
        list_title.map(Value::from).unwrap_or(Value::Null),
    );
    rec.insert(
        "captured_at".to_string(),
        Value::String(captured_at.to_string()),
    );
    Value::Object(rec)
}

fn to_item(list_label: &str, raw: &Value) -> Option<BackupItem> {
    let vid = raw
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let url = raw
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={vid}"));
    let tags: Vec<String> = [
        Some(list_label),
        raw.get("channel").and_then(|v| v.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .map(str::to_string)
    .collect();
    let mut item = BackupItem::new(format!("{list_label}:{vid}"), "video", raw.clone()).ok()?;
    item.title = raw
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.url = Some(url.clone());
    item.tags = tags;
    item.media = vec![MediaRef {
        url,
        kind: "video".to_string(),
        filename: None,
        mime: None,
        data: None,
    }];
    Some(item)
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

fn min_u32_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
    min: u32,
) -> Result<Option<u32>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    let n = v.as_u64().ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be a positive integer, got {v}"
        ))
    })?;
    if n < min as u64 {
        return Err(ConnectorError::Config(format!(
            "sources.<name>.{key} must be >= {min}, got {n}"
        )));
    }
    u32::try_from(n)
        .map(Some)
        .map_err(|_| ConnectorError::Config(format!("sources.<name>.{key} is too large, got {n}")))
}

impl Connector for YouTubeConnector {
    fn type_name(&self) -> &str {
        "youtube"
    }

    fn display_name(&self) -> &str {
        "YouTube (lists)"
    }

    fn description(&self) -> &str {
        "Your YouTube lists (Watch Later, Liked, history, playlists) via yt-dlp."
    }

    fn docs_url(&self) -> &str {
        "https://github.com/baileyrd/tubeyou"
    }

    fn setup_hint(&self) -> &str {
        "Google usually blocks sign-in in the capture browser. Easiest: set \
         cookies_from_browser (e.g. vivaldi, chrome, firefox, edge) to use your logged-in \
         browser's cookies — no login capture needed."
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

    fn auth_capture(&self) -> Option<&AuthCapture> {
        Some(&self.auth_capture)
    }

    fn export_profile(&self) -> Option<ExportProfile> {
        // tags holds [list_label, channel]; naming the channel
        // explicitly stops a channel page from merging with a
        // same-named playlist page.
        Some(ExportProfile {
            group_by: vec!["channel".to_string()],
            ..Default::default()
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: false, // no server-side delta -> every run is full
            supports_full_enumeration: true, // enables the soft-delete reconcile sweep
            supports_native_deletes: false, // removals from a list detected via reconcile
            produces_media: true,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: false,
            paginated: true,
            concurrency: "serial".to_string(), // yt-dlp extraction is resource-heavy
            ..Capabilities::default()
        }
    }

    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = bool_option(options, "watch_later")? {
            self.config.watch_later = v;
        }
        if let Some(v) = bool_option(options, "liked")? {
            self.config.liked = v;
        }
        if let Some(v) = bool_option(options, "history")? {
            self.config.history = v;
        }
        if let Some(v) = bool_option(options, "playlists")? {
            self.config.playlists = v;
        }
        if let Some(v) = min_u32_option(options, "max_history", 1)? {
            self.config.max_history = v;
        }
        if let Some(v) = u64_option(options, "extract_timeout")? {
            self.config.extract_timeout = v;
        }
        if let Some(v) = string_option(options, "cookies_from_browser")? {
            self.config.cookies_from_browser = Some(v);
        }
        Ok(())
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        if self.config.cookies_from_browser.is_none()
            && !self.secret_keys.contains(&self.config.cookies_file_env)
        {
            out.push(Err(ConnectorError::Config(format!(
                "cookies_file_env={:?} must be one of the declared secret_keys {:?}; set \
                 YOUTUBE_COOKIES_FILE in your .env, or set cookies_from_browser in the source \
                 config.",
                self.config.cookies_file_env, self.secret_keys
            ))));
            return Box::new(out.into_iter());
        }

        let cookiefile = if self.config.cookies_from_browser.is_none() {
            let path = match ctx.secrets.get(&self.config.cookies_file_env) {
                Ok(p) => p.to_string(),
                Err(e) => {
                    out.push(Err(e));
                    return Box::new(out.into_iter());
                }
            };
            if !std::path::Path::new(&path).exists() {
                out.push(Err(ConnectorError::Config(format!(
                    "YouTube cookies file {path} does not exist; export a Netscape \
                     cookies.txt from a logged-in browser, or set cookies_from_browser."
                ))));
                return Box::new(out.into_iter());
            }
            Some(path)
        } else {
            None
        };

        let mut targets: Vec<(String, String, Option<u32>)> = Vec::new();
        if self.config.history {
            targets.push((
                HISTORY.0.to_string(),
                HISTORY.1.to_string(),
                Some(self.config.max_history),
            ));
        }
        if self.config.watch_later {
            targets.push((WATCH_LATER.0.to_string(), WATCH_LATER.1.to_string(), None));
        }
        if self.config.liked {
            targets.push((LIKED.0.to_string(), LIKED.1.to_string(), None));
        }

        let mut live_ids = HashSet::new();
        let mut failed_lists = Vec::new();
        let mut seen_lists = 0u32;

        let process_list = |label: String,
                            source_url: &str,
                            playlist_end: Option<u32>,
                            out: &mut Vec<Result<FetchEvent, ConnectorError>>,
                            live_ids: &mut HashSet<String>,
                            seen_lists: &mut u32,
                            failed_lists: &mut Vec<String>| {
            match self.dump_list(&label, source_url, playlist_end, cookiefile.as_deref()) {
                Ok((title, entries)) => {
                    let captured_at = dbs_core::iso_z(chrono::Utc::now());
                    let last = entries.len().saturating_sub(1);
                    for (i, e) in entries.iter().enumerate() {
                        let rec =
                            entry_record((i + 1) as u32, e, &label, title.as_deref(), &captured_at);
                        if let Some(item) = to_item(&label, &rec) {
                            if live_ids.contains(item.external_id()) {
                                eprintln!(
                                    "youtube: skipping duplicate entry {}",
                                    item.external_id()
                                );
                            } else {
                                live_ids.insert(item.external_id().to_string());
                                out.push(Ok(FetchEvent::Item(item)));
                            }
                        }
                        if i == last && !entries.is_empty() {
                            *seen_lists += 1;
                            out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                                cursor: Cursor {
                                    value: serde_json::json!({"lists_done": *seen_lists}),
                                },
                                note: format!("after list {label}"),
                            })));
                        }
                    }
                }
                Err(msg) => {
                    eprintln!("youtube: {label} not accessible: {msg}");
                    failed_lists.push(label);
                }
            }
        };

        for (label, source_url, playlist_end) in &targets {
            process_list(
                label.clone(),
                source_url,
                *playlist_end,
                &mut out,
                &mut live_ids,
                &mut seen_lists,
                &mut failed_lists,
            );
        }

        if self.config.playlists {
            match self.discover_playlists(cookiefile.as_deref()) {
                Some(discovered) => {
                    for (label, url) in discovered {
                        process_list(
                            label,
                            &url,
                            None,
                            &mut out,
                            &mut live_ids,
                            &mut seen_lists,
                            &mut failed_lists,
                        );
                    }
                }
                None => failed_lists.push("playlists".to_string()),
            }
        }

        out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: serde_json::json!({"lists_done": seen_lists}),
            },
            note: "final".to_string(),
        })));

        if !failed_lists.is_empty() {
            // Partial enumeration: reconciling against incomplete data
            // would sweep everything the failed list(s) contain.
            eprintln!(
                "youtube: {} list(s) failed to load ({}) — partial enumeration, deletion \
                 detection skipped this run",
                failed_lists.len(),
                failed_lists.join(", ")
            );
        } else {
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
    use std::path::Path;

    fn ctx_with(cookies_secret: Option<&str>) -> RunContext {
        let mut store = HashMap::new();
        if let Some(c) = cookies_secret {
            store.insert("YOUTUBE_COOKIES_FILE".to_string(), c.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "youtube".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(store, vec!["YOUTUBE_COOKIES_FILE".to_string()]),
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
        }
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-connector-youtube-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fake_yt_dlp(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// A fake `yt-dlp` that branches on a URL substring in its own
    /// arguments (mirroring the fake-executable-on-disk pattern
    /// `dbs-research`'s YouTube search and `dbs-connector-vimeo`
    /// already use) so one script can stand in for every list this
    /// connector fetches.
    fn branching_fake_yt_dlp(dir: &Path) -> std::path::PathBuf {
        write_fake_yt_dlp(
            dir,
            "fake-yt-dlp.sh",
            r#"args="$*"
case "$args" in
  *"list=WL"*)
    echo '{"title":"Watch Later","entries":[{"id":"v1","title":"Video One","channel":"Chan"}]}'
    ;;
  *"list=LL"*)
    echo '{"title":"Liked","entries":[{"id":"v2","title":"Video Two","channel":"Chan"}]}'
    ;;
  *"ythistory"*)
    echo '{"title":"History","entries":[{"id":"v3","title":"Video Three","channel":"Chan"}]}'
    ;;
  *"feed/playlists"*)
    echo '{"entries":[{"id":"PLxyz","title":"My Playlist"}]}'
    ;;
  *"list=PLxyz"*)
    echo '{"title":"My Playlist","entries":[{"id":"v4","title":"Video Four","channel":"Chan"}]}'
    ;;
  *)
    echo '{"entries":[]}'
    ;;
esac
exit 0"#,
        )
    }

    fn config_with_browser_cookies() -> YouTubeConfig {
        YouTubeConfig {
            cookies_from_browser: Some("chrome".to_string()),
            playlists: false,
            ..Default::default()
        }
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<Result<FetchEvent, ConnectorError>> {
        iter.collect()
    }

    #[test]
    fn fetch_without_cookies_declared_or_from_browser_is_a_config_error() {
        let mut connector = YouTubeConnector::new(YouTubeConfig {
            cookies_file_env: "SOME_OTHER_VAR".to_string(),
            ..Default::default()
        });
        let ctx = ctx_with(None);
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_with_cookies_file_env_but_missing_secret_is_an_auth_error() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let ctx = ctx_with(None);
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn fetch_with_a_nonexistent_cookies_file_is_a_config_error() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let ctx = ctx_with(Some("/nonexistent/cookies.txt"));
        let result = events(connector.fetch(&ctx));
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn full_fetch_yields_videos_from_watch_later_and_liked_and_a_reconcile_marker() {
        let dir = temp_dir("full-fetch");
        let fake = branching_fake_yt_dlp(&dir);
        let mut connector = YouTubeConnector::new(config_with_browser_cookies())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(None);
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
        assert!(items.iter().any(|i| i.external_id() == "watch-later:v1"));
        assert!(items.iter().any(|i| i.external_id() == "liked:v2"));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("watch-later:v1") && marker.live_ids.contains("liked:v2"));
    }

    #[test]
    fn history_is_off_by_default_and_included_when_enabled() {
        let dir = temp_dir("history-off");
        let fake = branching_fake_yt_dlp(&dir);
        let mut connector = YouTubeConnector::new(config_with_browser_cookies())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(None);
        let evs: Vec<_> = events(connector.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert!(!evs.iter().any(
            |e| matches!(e, FetchEvent::Item(i) if i.external_id().starts_with("watch-history:"))
        ));

        let config = YouTubeConfig {
            history: true,
            watch_later: false,
            liked: false,
            ..config_with_browser_cookies()
        };
        let mut connector2 = YouTubeConnector::new(config).with_yt_dlp_bin(fake.to_string_lossy());
        let evs2: Vec<_> = events(connector2.fetch(&ctx))
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert!(evs2
            .iter()
            .any(|e| matches!(e, FetchEvent::Item(i) if i.external_id() == "watch-history:v3")));
    }

    #[test]
    fn a_failed_list_withholds_the_reconcile_marker_but_keeps_the_other_list() {
        let dir = temp_dir("failed-list");
        let fake = write_fake_yt_dlp(
            &dir,
            "fake-yt-dlp.sh",
            r#"case "$*" in
  *"list=WL"*) echo '{"title":"Watch Later","entries":[{"id":"v1","title":"Video One"}]}' ;;
  *"list=LL"*) echo "boom" >&2; exit 1 ;;
  *) echo '{"entries":[]}' ;;
esac
exit 0"#,
        );
        let mut connector = YouTubeConnector::new(config_with_browser_cookies())
            .with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(None);
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
            .filter(|e| matches!(e, FetchEvent::Item(_)))
            .collect();
        assert_eq!(items.len(), 1, "{evs:?}");
    }

    #[test]
    fn playlist_discovery_failure_marks_playlists_as_failed_without_aborting() {
        let dir = temp_dir("discovery-failed");
        let fake = write_fake_yt_dlp(
            &dir,
            "fake-yt-dlp.sh",
            r#"case "$*" in
  *"feed/playlists"*) echo "boom" >&2; exit 1 ;;
  *) echo '{"entries":[]}' ;;
esac
exit 0"#,
        );
        let config = YouTubeConfig {
            watch_later: false,
            liked: false,
            playlists: true,
            ..config_with_browser_cookies()
        };
        let mut connector = YouTubeConnector::new(config).with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(None);
        let result = events(connector.fetch(&ctx));
        assert!(!result.iter().any(|r| r.is_err()), "{result:?}");
        let evs: Vec<_> = result.into_iter().map(|r| r.unwrap()).collect();
        assert!(!evs
            .iter()
            .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))));
    }

    #[test]
    fn playlist_discovery_finds_and_dumps_each_playlist() {
        let dir = temp_dir("discovery-ok");
        let fake = branching_fake_yt_dlp(&dir);
        let config = YouTubeConfig {
            watch_later: false,
            liked: false,
            playlists: true,
            ..config_with_browser_cookies()
        };
        let mut connector = YouTubeConnector::new(config).with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(None);
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
        assert_eq!(items[0].external_id(), "playlist:My Playlist:v4");
        assert!(evs
            .iter()
            .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))));
    }

    #[test]
    fn a_duplicate_video_within_one_list_keeps_only_the_first_occurrence() {
        let dir = temp_dir("dedup");
        let fake = write_fake_yt_dlp(
            &dir,
            "fake-yt-dlp.sh",
            r#"case "$*" in
  *"list=WL"*) echo '{"title":"Watch Later","entries":[{"id":"v1","title":"First"},{"id":"v1","title":"Second"}]}' ;;
  *) echo '{"entries":[]}' ;;
esac
exit 0"#,
        );
        let config = YouTubeConfig {
            liked: false,
            ..config_with_browser_cookies()
        };
        let mut connector = YouTubeConnector::new(config).with_yt_dlp_bin(fake.to_string_lossy());
        let ctx = ctx_with(None);
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
        assert_eq!(items[0].title.as_deref(), Some("First"));
    }

    #[test]
    fn entry_record_falls_back_to_a_generated_watch_url_when_missing() {
        let e = serde_json::json!({"id": "abc123", "title": "T"});
        let rec = entry_record(1, &e, "watch-later", None, "2024-06-01T00:00:00Z");
        assert_eq!(rec["url"], "https://www.youtube.com/watch?v=abc123");
    }

    #[test]
    fn entry_record_maps_duration_to_duration_seconds() {
        let e = serde_json::json!({"id": "abc", "duration": 125});
        let rec = entry_record(1, &e, "liked", None, "2024-06-01T00:00:00Z");
        assert_eq!(rec["duration_seconds"], 125);
        assert!(rec.get("duration").is_none());
    }

    #[test]
    fn to_item_namespaces_the_external_id_by_list() {
        let raw = serde_json::json!({"id": "vid1", "title": "A Video", "channel": "Chan"});
        let item = to_item("liked", &raw).unwrap();
        assert_eq!(item.external_id(), "liked:vid1");
        assert_eq!(item.tags, vec!["liked".to_string(), "Chan".to_string()]);
        assert_eq!(item.media[0].kind, "video");
    }

    #[test]
    fn to_item_rejects_a_record_with_no_id() {
        let raw = serde_json::json!({"title": "orphan"});
        assert!(to_item("liked", &raw).is_none());
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = YouTubeConnector::new(YouTubeConfig::default());
        assert_eq!(connector.type_name(), "youtube");
        assert_eq!(
            connector.secret_keys(),
            &["YOUTUBE_COOKIES_FILE".to_string()]
        );
        assert!(!connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 1);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(connector.capabilities().supports_full_enumeration);
        assert_eq!(connector.capabilities().concurrency, "serial");
        assert_eq!(
            connector.volatile_fields(),
            &["captured_at".to_string(), "view_count".to_string()]
        );
        let capture = connector.auth_capture().unwrap();
        assert_eq!(capture.kind, "browser_cookies");
        assert!(connector.export_profile().is_some());
    }

    #[test]
    fn configure_applies_every_field_from_options() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let options = HashMap::from([
            ("watch_later".to_string(), serde_json::json!(false)),
            ("liked".to_string(), serde_json::json!(false)),
            ("history".to_string(), serde_json::json!(true)),
            ("playlists".to_string(), serde_json::json!(false)),
            ("max_history".to_string(), serde_json::json!(100)),
            ("extract_timeout".to_string(), serde_json::json!(30)),
            (
                "cookies_from_browser".to_string(),
                serde_json::json!("firefox"),
            ),
        ]);
        connector.configure(&options).unwrap();
        assert!(!connector.config.watch_later);
        assert!(!connector.config.liked);
        assert!(connector.config.history);
        assert!(!connector.config.playlists);
        assert_eq!(connector.config.max_history, 100);
        assert_eq!(connector.config.extract_timeout, 30);
        assert_eq!(
            connector.config.cookies_from_browser,
            Some("firefox".to_string())
        );
    }

    #[test]
    fn configure_with_no_matching_keys_leaves_defaults_untouched() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let defaults = YouTubeConfig::default();
        connector.configure(&HashMap::new()).unwrap();
        assert_eq!(connector.config.watch_later, defaults.watch_later);
        assert_eq!(connector.config.liked, defaults.liked);
        assert_eq!(connector.config.history, defaults.history);
        assert_eq!(connector.config.playlists, defaults.playlists);
        assert_eq!(connector.config.max_history, defaults.max_history);
        assert_eq!(connector.config.extract_timeout, defaults.extract_timeout);
        assert_eq!(
            connector.config.cookies_from_browser,
            defaults.cookies_from_browser
        );
    }

    #[test]
    fn configure_rejects_a_non_bool_watch_later() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let options = HashMap::from([("watch_later".to_string(), serde_json::json!("yes"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_max_history_below_1() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let options = HashMap::from([("max_history".to_string(), serde_json::json!(0))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_negative_extract_timeout() {
        let mut connector = YouTubeConnector::new(YouTubeConfig::default());
        let options = HashMap::from([("extract_timeout".to_string(), serde_json::json!(-1))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }
}
