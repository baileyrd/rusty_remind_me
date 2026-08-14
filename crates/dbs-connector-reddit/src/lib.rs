//! Reddit connector: backs up your *saved* posts and comments (issue
//! #96). Mirrors `dbs.connectors.reddit` in
//! baileyrd/Daily-Backup-System.
//!
//! Saved feeds are private and Reddit offers no OAuth-free REST
//! endpoint for them — but its **cookie-authenticated JSON listings**
//! work with a logged-in browser session: `GET /user/<name>/saved.json`
//! pages through the exact same data the site renders. The reference
//! loads a captured **persistent browser session** (Playwright) and
//! pages the JSON feed with a same-origin `fetch` evaluated **inside a
//! real browser page** on reddit.com — Reddit's edge fingerprints HTTP
//! clients, so a plain HTTP request (even with valid cookies) gets
//! 403'd; only a fetch evaluated in an actual Chromium page carries a
//! genuine TLS/HTTP2 fingerprint and client hints.
//!
//! **Acquisition (issue #187)** shells out to `scripts/acquire.py`
//! (embedded into the binary via `include_str!`, staged to a temp
//! file at run time) through issue #99's
//! `dbs_connector_support::python_launch::run_python_script` — there
//! is no Rust Playwright binding, so the actual browser driving
//! happens in a real Python subprocess, same split
//! `dbs-connector-support`'s module doc-comment describes. That
//! script's only job is browser automation and pagination: it hands
//! back the raw, undecoded `children` the saved-listing API returns,
//! and Rust does the actual record mapping via [`record_from_child`]
//! (below) — the same pure function this connector's tests already
//! exercised against fixture data before #187 existed, now reachable
//! from a real run too. `fetch()` still performs every check that
//! doesn't need a browser first (the `session_dir_env` config is
//! sane, the secret is set, the session directory actually exists on
//! disk) before ever spawning the script.
//!
//! Two consequences shape the design once acquisition exists: there's
//! no server-side `since` filter and no cheap delta (every run walks
//! the whole saved feed), and "un-saving" an item simply removes it
//! from the feed (no trash to poll). So this is a **full-enumeration**
//! source: `supports_incremental = false`, and a full run yields a
//! single [`ReconcileMarker`][dbs_core::ReconcileMarker] covering
//! every live id.
//!
//! Auth is a **path-valued secret**: `REDDIT_SESSION_DIR` points at
//! the Playwright persistent-context directory holding the logged-in
//! cookies.
//!
//! Reachable from a real `dbs backup reddit` run via the
//! `dbs-connector-reddit` subprocess binary (#164). Acquisition itself
//! is tested only against a fake acquisition-script stub (no real
//! Playwright/network access in CI, same convention
//! `dbs-connector-youtube`'s yt-dlp tests established) — everything
//! else is tested directly against the `Connector` trait and fixture
//! HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use dbs_connector_support::{find_python, run_python_script_using};
use dbs_core::export_profile::ExportProfile;
use dbs_core::parse_iso;
use dbs_core::{
    AuthCapture, BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor,
    FetchEvent, ItemKind, ManagedHttpClient, MediaRef, ReconcileMarker, RunContext,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RedditConfig {
    /// Optional cross-check only: the account actually backed up is
    /// whichever one the captured session is logged in as.
    pub username: Option<String>,
    pub include_types: Vec<String>,
    pub max_pages: u32,
    pub delay: f64,
    pub headless: bool,
    pub checkpoint_every: u32,
    /// Env var holding the path to the Playwright persistent-context
    /// directory (your logged-in session).
    pub session_dir_env: String,
    /// Opt-in: best-effort fetch of the outbound link a saved *post*
    /// points to (never attempted for comments).
    pub archive_outbound_link: bool,
}

impl Default for RedditConfig {
    fn default() -> Self {
        Self {
            username: None,
            include_types: vec!["post".to_string(), "comment".to_string()],
            max_pages: 100,
            delay: 2.0,
            headless: true,
            checkpoint_every: 200,
            session_dir_env: "REDDIT_SESSION_DIR".to_string(),
            archive_outbound_link: false,
        }
    }
}

pub struct RedditConnector {
    config: RedditConfig,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
    auth_capture: AuthCapture,
}

impl RedditConnector {
    pub fn new(config: RedditConfig) -> Self {
        Self {
            config,
            secret_keys: vec!["REDDIT_SESSION_DIR".to_string()],
            item_kinds: vec![
                ItemKind {
                    name: "post".to_string(),
                    display_name: "Post".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "comment".to_string(),
                    display_name: "Comment".to_string(),
                    description: String::new(),
                },
            ],
            // Besides the capture timestamp, Reddit's score and
            // comment count tick constantly on live threads — hashing
            // them turns nearly every saved item into a fresh
            // revision each run. We archive content, not live vote
            // metrics.
            volatile_fields: vec![
                "extracted_at".to_string(),
                "score".to_string(),
                "num_comments".to_string(),
            ],
            auth_capture: AuthCapture {
                kind: "browser_session".to_string(),
                secret_key: "REDDIT_SESSION_DIR".to_string(),
                login_url: "https://www.reddit.com/login/".to_string(),
                label: "Reddit login".to_string(),
                target_dir_option: String::new(),
                target_path: String::new(),
                per_source: true,
            },
        }
    }
}

/// Absolutizes a Reddit-relative permalink (`/r/rust/...` ->
/// `https://www.reddit.com/r/rust/...`); leaves an already-absolute
/// URL alone.
fn abs_permalink(permalink: &str) -> String {
    if !permalink.is_empty() && !permalink.starts_with("http") {
        format!("https://www.reddit.com{permalink}")
    } else {
        permalink.to_string()
    }
}

/// Maps one saved-listing child (`t3` post / `t1` comment) to the raw
/// record shape the rest of this connector consumes. `None` for any
/// other listing kind, or a child with no `name` (fullname) to key
/// off of.
fn record_from_child(child: &Value, extracted_at: &str) -> Option<Value> {
    let kind = child.get("kind").and_then(|v| v.as_str());
    if kind != Some("t3") && kind != Some("t1") {
        return None;
    }
    let d = child.get("data").cloned().unwrap_or(Value::Null);
    let fullname = d
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let permalink = abs_permalink(d.get("permalink").and_then(|v| v.as_str()).unwrap_or(""));
    let created = d
        .get("created_utc")
        .and_then(|v| v.as_f64())
        .and_then(|epoch| chrono::DateTime::<chrono::Utc>::from_timestamp(epoch as i64, 0))
        .map(dbs_core::iso_z)
        .unwrap_or_default();
    let subreddit = d
        .get("subreddit_name_prefixed")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let author = d.get("author").and_then(|v| v.as_str()).unwrap_or("");
    let score = d.get("score").and_then(|v| v.as_i64()).unwrap_or(0);

    if kind == Some("t3") {
        let mut outbound = d
            .get("url_overridden_by_dest")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                d.get("url")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("")
            .to_string();
        if outbound == permalink {
            // Self post: "outbound" is just itself.
            outbound.clear();
        }
        let thumb = d.get("thumbnail").and_then(|v| v.as_str()).unwrap_or("");
        // "self"/"default"/"nsfw"/... tokens are not real URLs.
        let thumb = if thumb.starts_with("http") { thumb } else { "" };
        Some(serde_json::json!({
            "id": fullname,
            "item_type": "post",
            "title": d.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            "subreddit": subreddit,
            "author": author,
            "permalink": permalink,
            "url": outbound,
            "score": score,
            "num_comments": d.get("num_comments").and_then(|v| v.as_i64()).unwrap_or(0),
            "flair": d.get("link_flair_text").and_then(|v| v.as_str()).unwrap_or(""),
            "created_utc": created,
            "selftext": d.get("selftext").and_then(|v| v.as_str()).unwrap_or(""),
            "comment_body": "",
            "thumbnail": thumb,
            "extracted_at": extracted_at,
        }))
    } else {
        Some(serde_json::json!({
            "id": fullname,
            "item_type": "comment",
            "title": "",
            "subreddit": subreddit,
            "author": author,
            "permalink": permalink,
            "url": "",
            "score": score,
            "num_comments": 0,
            "flair": "",
            "created_utc": created,
            "selftext": "",
            "comment_body": d.get("body").and_then(|v| v.as_str()).unwrap_or(""),
            "thumbnail": "",
            "extracted_at": extracted_at,
        }))
    }
}

/// A minimal MIME → extension map for `maybe_fetch_outbound_link`'s
/// filename. Intentionally local and small: `dbs-connector-support`'s
/// own doc comment defers a shared `ext_for_mime` "to whichever
/// media/export issue actually needs it" — this one only needs a
/// handful of common cases.
fn ext_for_mime(mime: Option<&str>) -> &'static str {
    match mime.map(|m| m.split(';').next().unwrap_or(m).trim()) {
        Some("text/html") => ".html",
        Some("application/pdf") => ".pdf",
        Some("image/jpeg") => ".jpg",
        Some("image/png") => ".png",
        Some("image/gif") => ".gif",
        Some("text/plain") => ".txt",
        Some("application/json") => ".json",
        _ => ".bin",
    }
}

/// Best-effort: fetch the outbound link a saved post points to.
/// Returns `None` on any failure — this must never abort the run,
/// since it's opportunistic enrichment and a dead link / timeout /
/// non-2xx must never fail the backup. A single hop with no header to
/// protect (an arbitrary external site, not Reddit's own API).
fn maybe_fetch_outbound_link(
    http: &RefCell<ManagedHttpClient>,
    ext_id: &str,
    url: &str,
) -> Option<MediaRef> {
    let response = http.borrow_mut().get(url).ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = response.bytes().ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(MediaRef {
        url: url.to_string(),
        kind: "archive".to_string(),
        filename: Some(format!("{ext_id}{}", ext_for_mime(mime.as_deref()))),
        mime,
        data: Some(bytes.to_vec()),
    })
}

/// Maps one raw saved-listing record (as produced by
/// [`record_from_child`]) to a `BackupItem`. `None` for a record with
/// no usable id.
fn to_item(
    cfg: &RedditConfig,
    http: Option<&RefCell<ManagedHttpClient>>,
    store_media: bool,
    raw: &Value,
) -> Option<BackupItem> {
    let ext_id = raw
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let kind = if raw.get("item_type").and_then(|v| v.as_str()) == Some("comment") {
        "comment"
    } else {
        "post"
    };
    let tags: Vec<String> = [
        raw.get("subreddit").and_then(|v| v.as_str()),
        raw.get("flair").and_then(|v| v.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .map(str::to_string)
    .collect();
    let mut media = Vec::new();
    if let Some(thumb) = raw
        .get("thumbnail")
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

    let outbound_url = raw.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let permalink = raw.get("permalink").and_then(|v| v.as_str()).unwrap_or("");
    if cfg.archive_outbound_link
        && store_media
        && kind == "post"
        && !outbound_url.is_empty()
        && outbound_url != permalink
    {
        if let Some(http) = http {
            if let Some(link_media) = maybe_fetch_outbound_link(http, ext_id, outbound_url) {
                media.push(link_media);
            }
        }
    }

    let mut item = BackupItem::new(ext_id, kind, raw.clone()).ok()?;
    item.title = raw
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    item.url = raw
        .get("permalink")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            raw.get("url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    item.body = raw
        .get("selftext")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            raw.get("comment_body")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    item.tags = tags;
    item.created_at = raw
        .get("created_utc")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item.media = media;
    Some(item)
}

// -- acquisition (Playwright-driven, via a Python subprocess; #187) -----

/// The embedded acquisition script — staged to a temp file at run time
/// and run through `dbs_connector_support::python_launch`. See the
/// module doc-comment for why the actual browser driving happens in a
/// separate Python process rather than in Rust.
const ACQUIRE_SCRIPT: &str = include_str!("../scripts/acquire.py");

/// How long a single acquisition run (browser launch + full saved-feed
/// walk) may take before being abandoned. `max_pages` defaults to 100
/// at `cfg.delay`'s default 2s/page, so a slow account can legitimately
/// take minutes; this is a generous outer bound against a genuinely
/// hung browser, not a tight budget.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(900);

/// The logged-in account name and every raw saved-listing `children`
/// entry the script paged through, undecoded — [`record_from_child`]
/// does the actual mapping.
#[derive(Debug)]
struct AcquireOutput {
    account: String,
    children: Vec<Value>,
}

fn script_error_to_connector_error(kind: &str, message: String) -> ConnectorError {
    match kind {
        "auth" => ConnectorError::Auth(message),
        "rate_limited" => ConnectorError::RateLimited(message),
        "config" => ConnectorError::Config(message),
        _ => ConnectorError::Transient(message),
    }
}

/// Runs the acquisition script under `interpreter` and parses its
/// single line of JSON result (see `scripts/acquire.py`'s own
/// doc-comment for the contract). Split from [`acquire`] so tests can
/// inject a fake interpreter/script instead of a real Python +
/// Playwright + live Reddit session — mirrors the reference's own
/// `_acquire` being overridden in its tests.
fn acquire_using(
    interpreter: &str,
    script: &Path,
    session_dir: &str,
    headless: bool,
    max_pages: u32,
    delay: f64,
    timeout: Duration,
) -> Result<AcquireOutput, ConnectorError> {
    let args = vec![
        session_dir.to_string(),
        headless.to_string(),
        max_pages.to_string(),
        delay.to_string(),
    ];
    let output = run_python_script_using(interpreter, script, &args, timeout).map_err(|e| {
        ConnectorError::Transient(format!(
            "reddit: failed to run the saved-feed acquisition script: {e}"
        ))
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout.lines().last().unwrap_or("").trim();
    if last_line.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ConnectorError::Transient(format!(
            "reddit: acquisition script produced no output (exit {:?}); stderr: {}",
            output.status.code(),
            stderr.trim()
        )));
    }
    let parsed: Value = serde_json::from_str(last_line).map_err(|e| {
        ConnectorError::Transient(format!(
            "reddit: acquisition script produced unparseable output ({e}): {last_line}"
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
            .unwrap_or("reddit: acquisition failed")
            .to_string();
        return Err(script_error_to_connector_error(kind, message));
    }
    Ok(AcquireOutput {
        account: parsed
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        children: parsed
            .get("children")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
}

/// Stages [`ACQUIRE_SCRIPT`] to a temp file and runs it through
/// whichever interpreter [`find_python`] resolves.
fn acquire(
    session_dir: &str,
    headless: bool,
    max_pages: u32,
    delay: f64,
) -> Result<AcquireOutput, ConnectorError> {
    let interpreter = find_python().ok_or_else(|| {
        ConnectorError::Config(
            "the Reddit connector needs Playwright; install it with `pip install playwright` \
             and run `playwright install chromium` (no python3/python interpreter found on \
             PATH)."
                .to_string(),
        )
    })?;
    let script_path = std::env::temp_dir().join(format!(
        "dbs-connector-reddit-acquire-{}-{:?}.py",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&script_path, ACQUIRE_SCRIPT).map_err(|e| {
        ConnectorError::Transient(format!(
            "reddit: failed to stage the acquisition script: {e}"
        ))
    })?;
    let result = acquire_using(
        interpreter,
        &script_path,
        session_dir,
        headless,
        max_pages,
        delay,
        ACQUIRE_TIMEOUT,
    );
    let _ = std::fs::remove_file(&script_path);
    result
}

impl Connector for RedditConnector {
    fn type_name(&self) -> &str {
        "reddit"
    }

    fn display_name(&self) -> &str {
        "Reddit (saved)"
    }

    fn description(&self) -> &str {
        "Your saved Reddit posts and comments, via a logged-in browser session."
    }

    fn docs_url(&self) -> &str {
        "https://github.com/baileyrd/reddit_saved_extractor"
    }

    fn setup_hint(&self) -> &str {
        "Click 'Reddit login' to capture a session: a browser opens, you log in, and you CLOSE \
         the window to finish. The account is auto-detected from the session."
    }

    fn secret_keys(&self) -> &[String] {
        &self.secret_keys
    }

    fn wants_managed_http(&self) -> bool {
        // The primary acquisition step is browser-driven and never
        // touches ctx.http; this is only for the opt-in
        // archive_outbound_link feature.
        true
    }

    fn volatile_fields(&self) -> &[String] {
        &self.volatile_fields
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn needs_playwright_browser(&self) -> bool {
        true
    }

    fn auth_capture(&self) -> Option<&AuthCapture> {
        Some(&self.auth_capture)
    }

    fn export_profile(&self) -> Option<ExportProfile> {
        Some(ExportProfile {
            // Reddit's real grouping axes are the subreddit and the
            // post flair; both currently land in the flat `tags`
            // list, where they're indistinguishable.
            group_by: vec!["subreddit".to_string(), "flair".to_string()],
            // A post's text is `selftext`; a saved comment's is
            // `comment_body`.
            body_from: vec!["selftext".to_string(), "comment_body".to_string()],
            ..Default::default()
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: false, // no server-side delta -> every run is full
            supports_full_enumeration: true, // enables the soft-delete reconcile sweep
            supports_native_deletes: false, // un-saves are detected via reconcile only
            produces_media: true,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: false,
            paginated: true,
            concurrency: "serial".to_string(), // drives a real browser
            ..Capabilities::default()
        }
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        if !self.secret_keys.contains(&self.config.session_dir_env) {
            out.push(Err(ConnectorError::Config(format!(
                "session_dir_env={:?} must be one of the declared secret_keys {:?}; set \
                 REDDIT_SESSION_DIR in your .env to the path of your logged-in Playwright \
                 session directory.",
                self.config.session_dir_env, self.secret_keys
            ))));
            return Box::new(out.into_iter());
        }
        if self.config.archive_outbound_link && !ctx.store_media {
            eprintln!(
                "reddit: archive_outbound_link is set but store_media is off for this source; \
                 no outbound links will be fetched (set store_media = true in dbs.toml to \
                 actually persist them)."
            );
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
                "Reddit session directory {session_dir} does not exist; capture a login once \
                 (the web UI's 'Reddit login' button) to create it."
            ))));
            return Box::new(out.into_iter());
        }

        let acquired = match acquire(
            &session_dir,
            self.config.headless,
            self.config.max_pages,
            self.config.delay,
        ) {
            Ok(a) => a,
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };
        if let Some(username) = &self.config.username {
            if !username.eq_ignore_ascii_case(&acquired.account) {
                eprintln!(
                    "reddit: config username {:?} does not match the logged-in account u/{} — \
                     backing up the logged-in account (saved feeds are owner-only, so the \
                     config value would fetch nothing).",
                    username, acquired.account
                );
            }
        }

        let extracted_at = dbs_core::iso_z(chrono::Utc::now());
        let mut live_ids: HashSet<String> = HashSet::new();
        let mut seen: u32 = 0;
        for child in &acquired.children {
            let Some(raw) = record_from_child(child, &extracted_at) else {
                continue;
            };
            let Some(item) = to_item(&self.config, ctx.http.as_ref(), ctx.store_media, &raw) else {
                continue;
            };
            // Still record the id so the reconcile sweep never deletes
            // an item that exists upstream but is merely out of
            // current `include_types` scope.
            live_ids.insert(item.external_id().to_string());
            if !self.config.include_types.is_empty()
                && !self.config.include_types.contains(&item.item_kind)
            {
                continue;
            }
            out.push(Ok(FetchEvent::Item(item)));
            seen += 1;
            if self.config.checkpoint_every > 0 && seen.is_multiple_of(self.config.checkpoint_every)
            {
                out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
                    cursor: Cursor {
                        value: serde_json::json!({"items_seen": seen}),
                    },
                    note: format!("after {seen} items"),
                })));
            }
        }
        if acquired.children.is_empty() {
            eprintln!(
                "reddit: authenticated as u/{} but the saved feed returned 0 items — either \
                 nothing is saved on this account, or Reddit served an empty listing.",
                acquired.account
            );
        }

        out.push(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: serde_json::json!({"items_seen": seen}),
            },
            note: "final".to_string(),
        })));
        out.push(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
            live_ids,
        ))));

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
        session_dir: Option<&str>,
        store_media: bool,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(d) = session_dir {
            store.insert("REDDIT_SESSION_DIR".to_string(), d.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "reddit".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(store, vec!["REDDIT_SESSION_DIR".to_string()]),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: Some(RefCell::new(http)),
        }
    }

    fn no_sleep_client() -> ManagedHttpClient {
        ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), |_| {})
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-connector-reddit-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn post_child(name: &str, title: &str, permalink: &str, url: &str, thumbnail: &str) -> Value {
        serde_json::json!({
            "kind": "t3",
            "data": {
                "name": name,
                "title": title,
                "subreddit_name_prefixed": "r/rust",
                "author": "someone",
                "permalink": permalink,
                "url": url,
                "url_overridden_by_dest": url,
                "score": 42,
                "num_comments": 3,
                "link_flair_text": "Discussion",
                "created_utc": 1_700_000_000.0,
                "selftext": "",
                "thumbnail": thumbnail,
            }
        })
    }

    fn comment_child(name: &str, body: &str) -> Value {
        serde_json::json!({
            "kind": "t1",
            "data": {
                "name": name,
                "subreddit_name_prefixed": "r/rust",
                "author": "someone",
                "permalink": "/r/rust/comments/abc/x/def/",
                "score": 5,
                "created_utc": 1_700_000_000.0,
                "body": body,
            }
        })
    }

    #[test]
    fn fetch_with_an_undeclared_session_dir_env_is_a_config_error() {
        let config = RedditConfig {
            session_dir_env: "SOME_OTHER_VAR".to_string(),
            ..Default::default()
        };
        let mut connector = RedditConnector::new(config);
        let ctx = ctx_with(no_sleep_client(), None, false);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_without_the_session_dir_secret_set_is_an_auth_error() {
        let mut connector = RedditConnector::new(RedditConfig::default());
        let ctx = ctx_with(no_sleep_client(), None, false);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn fetch_with_a_nonexistent_session_dir_is_a_config_error() {
        let mut connector = RedditConnector::new(RedditConfig::default());
        let ctx = ctx_with(
            no_sleep_client(),
            Some("/nonexistent/path/that/should/not/exist"),
            false,
        );
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    /// With a valid (but real-session-less) directory, `fetch()` now
    /// actually runs the acquisition script (#187) instead of
    /// returning a static "blocked" error. It still can't succeed in
    /// a sandbox with no live Playwright/Reddit session, but exactly
    /// what it fails with depends on the environment (no python3 on
    /// `PATH` vs. python3 present but Playwright not installed vs.
    /// Playwright installed but no real browser) — so this only
    /// asserts the environment-independent invariant: a single error
    /// result, nothing yielded.
    #[test]
    fn fetch_with_a_valid_but_empty_session_dir_fails_cleanly() {
        let dir = temp_dir("valid-session");
        let mut connector = RedditConnector::new(RedditConfig::default());
        let ctx = ctx_with(no_sleep_client(), Some(&dir.to_string_lossy()), false);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1, "{result:?}");
        assert!(result[0].is_err(), "{result:?}");
    }

    // -- acquire_using: exercised against a fake stub "interpreter"
    // (mirrors dbs_connector_support::python_launch's own tests and
    // dbs-connector-youtube's fake-yt-dlp convention) so the JSON
    // result contract between the Python script and this Rust code is
    // tested without needing real Python, Playwright, or network
    // access. ---------------------------------------------------------

    fn write_stub_script(
        dir: &std::path::Path,
        stdout: &str,
        exit_code: i32,
    ) -> std::path::PathBuf {
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
    fn acquire_using_parses_a_successful_result() {
        let dir = temp_dir("acquire-success");
        let script = write_stub_script(
            &dir,
            r#"{"ok": true, "account": "someone", "children": [{"kind": "t3"}]}"#,
            0,
        );
        let result = acquire_using(
            "/bin/sh",
            &script,
            "/some/dir",
            true,
            10,
            0.0,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(result.account, "someone");
        assert_eq!(result.children.len(), 1);
    }

    fn acquire_with_error_kind(
        dir: &std::path::Path,
        kind: &str,
    ) -> Result<AcquireOutput, ConnectorError> {
        let script = write_stub_script(
            dir,
            &format!(r#"{{"ok": false, "kind": "{kind}", "message": "boom"}}"#),
            1,
        );
        acquire_using(
            "/bin/sh",
            &script,
            "/some/dir",
            true,
            10,
            0.0,
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
    fn acquire_using_treats_unparseable_output_as_transient() {
        let dir = temp_dir("acquire-garbage");
        let script = write_stub_script(&dir, "not json at all", 1);
        let result = acquire_using(
            "/bin/sh",
            &script,
            "/some/dir",
            true,
            10,
            0.0,
            Duration::from_secs(5),
        );
        match result {
            Err(ConnectorError::Transient(msg)) => assert!(msg.contains("unparseable"), "{msg}"),
            other => panic!("expected a Transient error, got {other:?}"),
        }
    }

    #[test]
    fn acquire_using_treats_no_output_as_transient() {
        let dir = temp_dir("acquire-empty");
        let script = dir.join("empty.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let result = acquire_using(
            "/bin/sh",
            &script,
            "/some/dir",
            true,
            10,
            0.0,
            Duration::from_secs(5),
        );
        match result {
            Err(ConnectorError::Transient(msg)) => assert!(msg.contains("no output"), "{msg}"),
            other => panic!("expected a Transient error, got {other:?}"),
        }
    }

    #[test]
    fn acquire_using_passes_config_through_as_positional_arguments() {
        let dir = temp_dir("acquire-args");
        // Echo the argv it was given back as the "account" field, so
        // the assertion proves session_dir/headless/max_pages/delay
        // all reach the script positionally in that order.
        let path = dir.join("echo_args.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\necho \"{\\\"ok\\\": true, \\\"account\\\": \\\"$1|$2|$3|$4\\\", \
             \\\"children\\\": []}\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let result = acquire_using(
            "/bin/sh",
            &path,
            "/my/session/dir",
            false,
            42,
            1.5,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(result.account, "/my/session/dir|false|42|1.5");
    }

    #[test]
    fn record_from_child_maps_a_post() {
        let child = post_child(
            "t3_abc",
            "Hello Rust",
            "/r/rust/comments/abc/hello_rust/",
            "https://example.com/article",
            "https://thumb.example.com/x.jpg",
        );
        let rec = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        assert_eq!(rec["id"], "t3_abc");
        assert_eq!(rec["item_type"], "post");
        assert_eq!(rec["title"], "Hello Rust");
        assert_eq!(rec["url"], "https://example.com/article");
        assert_eq!(
            rec["permalink"],
            "https://www.reddit.com/r/rust/comments/abc/hello_rust/"
        );
        assert_eq!(rec["thumbnail"], "https://thumb.example.com/x.jpg");
        assert_eq!(rec["created_utc"], "2023-11-14T22:13:20Z");
    }

    #[test]
    fn record_from_child_maps_a_comment() {
        let child = comment_child("t1_xyz", "great point");
        let rec = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        assert_eq!(rec["id"], "t1_xyz");
        assert_eq!(rec["item_type"], "comment");
        assert_eq!(rec["comment_body"], "great point");
        assert_eq!(rec["title"], "");
    }

    #[test]
    fn record_from_child_skips_a_non_post_comment_listing_kind() {
        let child = serde_json::json!({"kind": "t5", "data": {"name": "t5_sub"}});
        assert!(record_from_child(&child, "2024-06-01T00:00:00Z").is_none());
    }

    #[test]
    fn record_from_child_skips_a_child_with_no_fullname() {
        let child = serde_json::json!({"kind": "t3", "data": {"title": "no name field"}});
        assert!(record_from_child(&child, "2024-06-01T00:00:00Z").is_none());
    }

    #[test]
    fn record_from_child_treats_a_self_post_outbound_url_as_empty() {
        let permalink = "/r/rust/comments/abc/self_post/";
        let child = post_child(
            "t3_self",
            "A self post",
            permalink,
            "https://www.reddit.com/r/rust/comments/abc/self_post/",
            "self",
        );
        let rec = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        assert_eq!(rec["url"], "");
        assert_eq!(rec["thumbnail"], "");
    }

    #[test]
    fn abs_permalink_prefixes_a_relative_path_and_leaves_absolute_urls_alone() {
        assert_eq!(
            abs_permalink("/r/rust/comments/abc/"),
            "https://www.reddit.com/r/rust/comments/abc/"
        );
        assert_eq!(
            abs_permalink("https://www.reddit.com/r/rust/comments/abc/"),
            "https://www.reddit.com/r/rust/comments/abc/"
        );
        assert_eq!(abs_permalink(""), "");
    }

    #[test]
    fn to_item_maps_a_post_record() {
        let child = post_child(
            "t3_abc",
            "Hello Rust",
            "/r/rust/comments/abc/hello_rust/",
            "https://example.com/article",
            "https://thumb.example.com/x.jpg",
        );
        let raw = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        let item = to_item(&RedditConfig::default(), None, false, &raw).unwrap();
        assert_eq!(item.item_kind, "post");
        assert_eq!(item.title.as_deref(), Some("Hello Rust"));
        assert_eq!(
            item.url.as_deref(),
            Some("https://www.reddit.com/r/rust/comments/abc/hello_rust/")
        );
        assert_eq!(
            item.tags,
            vec!["r/rust".to_string(), "Discussion".to_string()]
        );
        assert_eq!(item.media.len(), 1);
        assert_eq!(item.media[0].kind, "image");
    }

    #[test]
    fn to_item_maps_a_comment_record() {
        let child = comment_child("t1_xyz", "great point");
        let raw = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        let item = to_item(&RedditConfig::default(), None, false, &raw).unwrap();
        assert_eq!(item.item_kind, "comment");
        assert_eq!(item.title, None);
        assert_eq!(item.body.as_deref(), Some("great point"));
    }

    #[test]
    fn to_item_rejects_a_record_with_no_id() {
        let raw = serde_json::json!({"item_type": "post", "title": "orphan"});
        assert!(to_item(&RedditConfig::default(), None, false, &raw).is_none());
    }

    #[test]
    fn to_item_fetches_outbound_link_when_enabled_and_store_media_is_on() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/article")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html>hi</html>")
            .create();
        let child = post_child(
            "t3_abc",
            "Hello Rust",
            "/r/rust/comments/abc/hello_rust/",
            &format!("{}/article", server.url()),
            "",
        );
        let raw = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        let cfg = RedditConfig {
            archive_outbound_link: true,
            ..Default::default()
        };
        let http = RefCell::new(no_sleep_client());
        let item = to_item(&cfg, Some(&http), true, &raw).unwrap();
        let link_media = item.media.iter().find(|m| m.kind == "archive").unwrap();
        assert_eq!(link_media.filename.as_deref(), Some("t3_abc.html"));
        assert_eq!(
            link_media.data.as_deref(),
            Some(b"<html>hi</html>".as_slice())
        );
    }

    #[test]
    fn to_item_skips_outbound_link_when_store_media_is_off() {
        let server = mockito::Server::new();
        let child = post_child(
            "t3_abc",
            "Hello Rust",
            "/r/rust/comments/abc/hello_rust/",
            &format!("{}/article", server.url()),
            "",
        );
        let raw = record_from_child(&child, "2024-06-01T00:00:00Z").unwrap();
        let cfg = RedditConfig {
            archive_outbound_link: true,
            ..Default::default()
        };
        let http = RefCell::new(no_sleep_client());
        let item = to_item(&cfg, Some(&http), false, &raw).unwrap();
        assert!(!item.media.iter().any(|m| m.kind == "archive"));
    }

    #[test]
    fn ext_for_mime_maps_known_types_and_falls_back_to_bin() {
        assert_eq!(ext_for_mime(Some("text/html; charset=utf-8")), ".html");
        assert_eq!(ext_for_mime(Some("application/pdf")), ".pdf");
        assert_eq!(ext_for_mime(Some("image/weird")), ".bin");
        assert_eq!(ext_for_mime(None), ".bin");
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = RedditConnector::new(RedditConfig::default());
        assert_eq!(connector.type_name(), "reddit");
        assert_eq!(connector.secret_keys(), &["REDDIT_SESSION_DIR".to_string()]);
        assert!(connector.wants_managed_http());
        assert!(connector.needs_playwright_browser());
        assert_eq!(connector.item_kinds().len(), 2);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_incremental);
        assert!(connector.capabilities().supports_full_enumeration);
        assert!(!connector.capabilities().supports_native_deletes);
        assert_eq!(connector.capabilities().concurrency, "serial");
        let capture = connector.auth_capture().unwrap();
        assert_eq!(capture.kind, "browser_session");
        assert_eq!(capture.secret_key, "REDDIT_SESSION_DIR");
        assert!(connector.export_profile().is_some());
    }
}
