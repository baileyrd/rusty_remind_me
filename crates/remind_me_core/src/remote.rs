//! Remote MCP connector configuration and token resolution — the FT-05
//! (secret-path/bearer) slice of `#57`, plus the FT-07 OAuth *state*
//! surface (`#86`): [`OAuthStateStore`] and the issuer/state-file fields on
//! [`RemoteConfig`]/[`RemoteStatus`].
//!
//! The Streamable HTTP transport itself, and the actual OAuth authorization
//! server (PKCE verification, token issuance, the `/consent` flow), live in
//! the separate `remind_me_remote` crate (tokio/axum/rmcp), not here. This
//! module stays in `remind_me_core` deliberately: `remind_me_mcp`'s
//! `remind_me_server_status` tool needs to report whether the connector is
//! configured, and its `remind_me_revoke_clients` tool needs to list/revoke
//! OAuth clients — both from a crate that must not pull tokio/axum/rmcp in
//! just to read an env var, stat a file, or edit a small JSON document.
//! Splitting "state a sync caller can read and mutate" from "the async
//! server that serves it" mirrors [`crate::webhook::WebhookStatus`] and
//! [`crate::sync::worker::SyncWorkerStatus`], which draw the same line
//! between their status structs (here, sync, dependency-light) and their
//! actual listener threads (also here, but could just as easily not be).
//! [`OAuthStateStore`] is the same idea applied to OAuth: the reference's
//! `remind_me_revoke_clients` tool (`tools/admin.py`) operates on
//! `OAuthStateStore` directly, in the synchronous stdio process, and the
//! *running* remote server re-reads the same file on every token check —
//! that cross-process-without-an-API design is what this split preserves.
//!
//! # Token generation
//!
//! This workspace has no vetted `rand`-style CSPRNG dependency anywhere —
//! `remind_me_api`'s own `resolve_api_key` doc explicitly declined to
//! invent one for a case where an unauthenticated fallback exists. That
//! fallback doesn't exist here: the token doubles as the connector URL's
//! secret path segment (`/mcp/<token>`), so an unconfigured connector must
//! still get a real credential, not silently no-op. Rather than adding a
//! new dependency for one call site, this reuses the CSPRNG already
//! compiled into the workspace via `uuid`'s `v4` feature (backed by
//! `getrandom`, an OS-entropy source — not a hand-rolled RNG). Two
//! concatenated v4 UUIDs give roughly 244 bits of entropy, comfortably
//! above the reference's own `secrets.token_urlsafe(32)` (256 bits).
//! [`generate_token`] is `pub` (rather than private, as it was pre-`#86`) so
//! `remind_me_remote`'s OAuth module can mint auth codes, access tokens,
//! refresh tokens, and consent transaction ids the same way, instead of
//! duplicating the reasoning above next to a second copy of the same code.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Set to `1`/`true`/`yes` (or pass `--serve-remote` at the CLI layer) to
/// expose the MCP server as a remote connector. Default off — matches the
/// reference's `REMIND_ME_REMOTE_MCP`.
pub const REMOTE_ENABLED_ENV: &str = "REMIND_ME_REMOTE_MCP";
pub const REMOTE_HOST_ENV: &str = "REMIND_ME_REMOTE_HOST";
pub const REMOTE_PORT_ENV: &str = "REMIND_ME_REMOTE_PORT";
/// Connector token. Always wins over the persisted file when set.
pub const REMOTE_TOKEN_ENV: &str = "REMIND_ME_REMOTE_TOKEN";
/// Overrides where the auto-generated token is persisted. Mainly for tests —
/// see [`default_token_file`] for the real default.
pub const REMOTE_TOKEN_FILE_ENV: &str = "REMIND_ME_REMOTE_TOKEN_FILE";
/// Public HTTPS origin the OAuth authorization server (FT-07) advertises
/// itself as. Unset (the default) means OAuth is off and the connector
/// stays FT-05 secret-path/bearer only. Matches the reference's
/// `REMIND_ME_REMOTE_ISSUER` — never derive this from the `Host` header
/// (see `remind_me_remote::oauth::issuer`'s module doc for why).
pub const REMOTE_ISSUER_ENV: &str = "REMIND_ME_REMOTE_ISSUER";
/// Overrides where the OAuth client/token state file is persisted. Mainly
/// for tests — see [`default_oauth_state_file`] for the real default.
pub const REMOTE_OAUTH_STATE_FILE_ENV: &str = "REMIND_ME_REMOTE_OAUTH_STATE_FILE";

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8768;

/// Where to bind the remote MCP connector. Loopback by default — widening
/// this without a tunnel in front is the caller's job to warn about (see
/// `remind_me_remote::server::warn_if_widened`, which this type has no
/// reason to know about).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub host: String,
    pub port: u16,
    /// `REMIND_ME_REMOTE_ISSUER`, trimmed and blank-filtered. `Some` turns
    /// OAuth mode (FT-07) on in `remind_me_remote::server::build_router`.
    pub issuer: Option<String>,
}

impl RemoteConfig {
    /// Reads env vars at call time (not cached) so tests can set/unset them
    /// per case, matching this workspace's existing config conventions
    /// (e.g. `webhook::WebhookConfig::from_env`).
    pub fn from_env() -> Self {
        let host = std::env::var(REMOTE_HOST_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port = std::env::var(REMOTE_PORT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let issuer = std::env::var(REMOTE_ISSUER_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        Self { host, port, issuer }
    }
}

/// Whether `REMIND_ME_REMOTE_MCP` (or the CLI's `--serve-remote`, which sets
/// it before this is read) asked for the connector to run.
pub fn remote_enabled() -> bool {
    std::env::var(REMOTE_ENABLED_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Default token file location: `~/.remind_me/connector_token`.
///
/// The reference persists it under its own `MEMORY_DIR` (`~/.remind-me`,
/// hyphenated) — this port has no equivalent single "memory directory"
/// concept to reuse, so this instead matches the directory this port's own
/// `rusty-remind-me configure` subcommand already writes the database under
/// (`crates/remind_me_cli/src/main.rs`'s `~/.remind_me`, underscored), to
/// avoid inventing a third convention.
fn default_token_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".remind_me")
        .join("connector_token")
}

/// The effective token file path: [`REMOTE_TOKEN_FILE_ENV`] if set and
/// non-blank, otherwise [`default_token_file`].
pub fn token_file_path() -> PathBuf {
    std::env::var(REMOTE_TOKEN_FILE_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(default_token_file)
}

/// Default OAuth state file location: `~/.remind_me/oauth.json`, sibling of
/// [`default_token_file`] — same "no `MEMORY_DIR` equivalent to reuse"
/// reasoning as that function's own doc.
fn default_oauth_state_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".remind_me")
        .join("oauth.json")
}

/// The effective OAuth state file path: [`REMOTE_OAUTH_STATE_FILE_ENV`] if
/// set and non-blank, otherwise [`default_oauth_state_file`].
pub fn oauth_state_file_path() -> PathBuf {
    std::env::var(REMOTE_OAUTH_STATE_FILE_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(default_oauth_state_file)
}

/// Mint a high-entropy token (see this module's doc for why two UUIDs
/// rather than a new CSPRNG dependency). Used for the connector token
/// itself and, by `remind_me_remote`'s OAuth module, for auth codes, access
/// tokens, refresh tokens, and consent transaction ids.
pub fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Resolve the effective connector token (SE-01 parity with the reference's
/// `resolve_connector_token`, and this port's own `resolve_api_key`
/// precedent):
///
/// 1. [`REMOTE_TOKEN_ENV`] — always wins when set and non-blank.
/// 2. The token persisted at [`token_file_path`].
/// 3. First use: generate one, persist it (`0600` on unix), and return it.
///
/// A token file that can be neither read nor written yields an ephemeral
/// per-process token instead of ever leaving the endpoint unauthenticated.
pub fn resolve_connector_token() -> String {
    if let Ok(env_token) = std::env::var(REMOTE_TOKEN_ENV) {
        let env_token = env_token.trim().to_string();
        if !env_token.is_empty() {
            return env_token;
        }
    }

    let file = token_file_path();
    if let Ok(existing) = fs::read_to_string(&file) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }

    let token = generate_token();
    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&file, format!("{token}\n")).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o600));
        }
    }
    token
}

/// What `remind_me_server_status` reports for the remote MCP connector.
///
/// Mirrors [`crate::webhook::WebhookStatus`] / [`crate::sync::worker::SyncWorkerStatus`]'s
/// shape: plain, `Serialize`-able, gathered by a sync free function. Matches
/// the reference's `get_remote_status()` field-for-field, including the
/// FT-07 OAuth fields (`oauth_enabled`, `issuer`, `oauth_state_file`,
/// `oauth_clients`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub token_file: String,
    pub token_configured: bool,
    pub oauth_enabled: bool,
    pub issuer: Option<String>,
    pub oauth_state_file: String,
    pub oauth_clients: usize,
}

/// Gather [`RemoteStatus`]. Reads env vars and stats the token/state files
/// at call time (not cached) so tests can monkeypatch them, same as
/// [`RemoteConfig::from_env`].
pub fn remote_status() -> RemoteStatus {
    let config = RemoteConfig::from_env();
    let token_file = token_file_path();
    let token_configured = std::env::var(REMOTE_TOKEN_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || token_file.is_file();
    let oauth_state_file = oauth_state_file_path();
    let oauth_clients = OAuthStateStore::new(oauth_state_file.clone())
        .list_clients()
        .len();
    RemoteStatus {
        enabled: remote_enabled(),
        host: config.host,
        port: config.port,
        token_file: token_file.to_string_lossy().to_string(),
        token_configured,
        oauth_enabled: config.issuer.is_some(),
        issuer: config.issuer,
        oauth_state_file: oauth_state_file.to_string_lossy().to_string(),
        oauth_clients,
    }
}

// ---------------------------------------------------------------------------
// OAuth state (FT-07, `#86`) — clients + token hashes, shared with the
// running remote server via the same JSON file.
// ---------------------------------------------------------------------------

/// Which token bucket a stored/looked-up token hash belongs to. A typed
/// alternative to the reference's stringly-typed `kind` parameter
/// (`"access_tokens"` / `"refresh_tokens"`) — same two buckets, no risk of a
/// typo'd literal silently missing every lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Refresh,
}

/// Counts of tokens a bulk operation touched, keyed the same way the
/// reference's `_drop_tokens` return dict is (`access_tokens`/`refresh_tokens`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenCounts {
    pub access_tokens: usize,
    pub refresh_tokens: usize,
}

/// Summary of one registered client for `remind_me_revoke_clients`' listing
/// — mirrors the reference's `OAuthStateStore.list_clients()` dict shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthClientSummary {
    pub client_id: String,
    pub client_name: Option<String>,
    pub client_id_issued_at: Option<i64>,
    pub redirect_uris: Option<Vec<String>>,
    pub access_tokens: usize,
    pub refresh_tokens: usize,
}

/// What revoking a client reports — the reference's `revoke_client` return
/// dict (`client_id`, `client_name`, plus the counts of tokens dropped).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthRevokeSummary {
    pub client_id: String,
    pub client_name: Option<String>,
    pub access_tokens: usize,
    pub refresh_tokens: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OAuthState {
    #[serde(default)]
    clients: Map<String, Value>,
    #[serde(default)]
    access_tokens: Map<String, Value>,
    #[serde(default)]
    refresh_tokens: Map<String, Value>,
}

impl OAuthState {
    fn tokens(&self, kind: TokenKind) -> &Map<String, Value> {
        match kind {
            TokenKind::Access => &self.access_tokens,
            TokenKind::Refresh => &self.refresh_tokens,
        }
    }

    fn tokens_mut(&mut self, kind: TokenKind) -> &mut Map<String, Value> {
        match kind {
            TokenKind::Access => &mut self.access_tokens,
            TokenKind::Refresh => &mut self.refresh_tokens,
        }
    }

    /// Remove every token (both buckets) belonging to `client_id`, mutating
    /// `self`. Mirrors the reference's `OAuthStateStore._drop_tokens`.
    fn drop_tokens(&mut self, client_id: &str) -> OAuthTokenCounts {
        let belongs =
            |meta: &Value| meta.get("client_id").and_then(Value::as_str) == Some(client_id);
        let access_doomed: Vec<String> = self
            .access_tokens
            .iter()
            .filter(|(_, meta)| belongs(meta))
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in &access_doomed {
            self.access_tokens.remove(hash);
        }
        let refresh_doomed: Vec<String> = self
            .refresh_tokens
            .iter()
            .filter(|(_, meta)| belongs(meta))
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in &refresh_doomed {
            self.refresh_tokens.remove(hash);
        }
        OAuthTokenCounts {
            access_tokens: access_doomed.len(),
            refresh_tokens: refresh_doomed.len(),
        }
    }
}

/// SHA-256 hex digest of a token — what the state file stores instead of
/// the raw secret (mirrors the reference's `_hash_token`).
fn hash_token(token: &str) -> String {
    sha256::digest(token)
}

/// JSON-file persistence for OAuth state (FT-07, `#86`).
///
/// Layout: `{"clients": {client_id: <RFC 7591 record>}, "access_tokens":
/// {sha256_hex: {client_id, scopes, expires_at, resource}}, "refresh_tokens":
/// {sha256_hex: {client_id, scopes, expires_at}}}` — a direct port of the
/// reference's `OAuthStateStore` layout, so the state file this crate writes
/// and the one the reference writes are structurally interchangeable. The
/// file is created with `0600` permissions (SE-01 conventions, matching
/// `resolve_connector_token`'s own token file) and re-read on every
/// operation, so a mutation from another process (the remote server
/// checking a bearer token, or `remind_me_revoke_clients` in the stdio
/// process) is visible immediately, with no cache to invalidate. The
/// in-process [`Mutex`] only serializes this process's own read-modify-write
/// cycles; cross-process locking is out of scope for a single-user store,
/// same as the reference.
///
/// Client and token-metadata records are stored as raw [`Value`] rather than
/// a fixed struct, deliberately: this crate has no reason to know the shape
/// of an RFC 7591 client record or a token's `scopes`/`resource` fields —
/// only `remind_me_remote`'s OAuth provider does. Keeping the store
/// schema-agnostic here is what lets `remind_me_core` (and thus
/// `remind_me_mcp`'s sync `remind_me_revoke_clients` tool) read/revoke
/// clients without depending on `remind_me_remote` at all.
pub struct OAuthStateStore {
    path: PathBuf,
    lock: Mutex<()>,
    /// Set once this process has successfully persisted state to `path` at
    /// least once (see [`Self::write`]). Lets [`Self::read`] tell "the file
    /// has genuinely never existed" (fine to read as empty immediately)
    /// apart from "this process just wrote it and a same-process read is
    /// transiently not seeing that write yet" (worth a few short retries
    /// before falling back to empty) — see `read`'s doc for why that
    /// second case is a real, observed failure mode here, not defensive
    /// paranoia.
    has_written: std::sync::atomic::AtomicBool,
}

impl OAuthStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
            has_written: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_once(&self) -> Option<OAuthState> {
        let raw = fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Load state from disk, tolerating a missing or corrupt file (reads
    /// as empty rather than propagating an error — same tolerance as the
    /// reference's `_read`).
    ///
    /// If this process has already persisted state here at least once
    /// ([`Self::has_written`]) and this read comes back missing/corrupt
    /// anyway, that's retried a handful of times with a short backoff
    /// before giving up: this was observed, running this crate's own test
    /// suite under heavy parallel filesystem load, to happen for a file
    /// this exact process had just written and verified moments earlier —
    /// a same-process read-after-write becoming transiently invisible,
    /// which a bare `fs::read_to_string` retry (rather than trusting the
    /// first miss) reliably resolves. A store that has never successfully
    /// written skips the retries and reads a missing file as empty
    /// immediately, matching the reference's own behavior for a brand-new
    /// state file.
    fn read(&self) -> OAuthState {
        if let Some(state) = self.read_once() {
            return state;
        }
        if self.has_written.load(std::sync::atomic::Ordering::Acquire) {
            for attempt in 0..8u32 {
                std::thread::sleep(std::time::Duration::from_millis(1 << attempt.min(5)));
                if let Some(state) = self.read_once() {
                    return state;
                }
            }
        }
        OAuthState::default()
    }

    /// Persist `state`, creating the parent directory if needed and setting
    /// `0600` permissions on unix. Best-effort: an unwritable path is
    /// silently dropped after a few immediate retries (mirrors the
    /// reference logging-and-continuing rather than propagating an
    /// `OSError`) — callers that already hold the client/token in memory
    /// (e.g. the in-flight auth code exchange) keep working within this
    /// process even if the file can't be written. The retry loop exists for
    /// a real, observed failure mode rather than defensive paranoia: a
    /// freshly-created parent directory can transiently fail a same-tick
    /// `create_dir_all`-then-`write` under heavy concurrent filesystem
    /// activity (seen running this crate's own test suite in parallel,
    /// each test creating its own state file), and silently losing a just-
    /// issued token to that race would be a real, hard-to-diagnose bug.
    fn write(&self, state: &OAuthState) {
        let Ok(body) = serde_json::to_string_pretty(state) else {
            return;
        };
        let body = format!("{body}\n");
        for attempt in 0..10 {
            if let Some(parent) = self.path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if Self::write_and_sync(&self.path, body.as_bytes()).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
                }
                // Read back what was just written rather than trusting a
                // successful write alone -- observed, under heavy
                // concurrent filesystem activity, to occasionally return
                // success for a write a same-process readback then does not
                // see. Retrying the whole create-dir/write/sync/verify cycle
                // (rather than trusting the first "successful" write) is
                // what actually closes that window.
                if fs::read_to_string(&self.path)
                    .map(|on_disk| on_disk == body)
                    .unwrap_or(false)
                {
                    self.has_written
                        .store(true, std::sync::atomic::Ordering::Release);
                    return;
                }
            }
            if attempt < 9 {
                std::thread::sleep(std::time::Duration::from_millis(1 << attempt.min(6)));
            }
        }
    }

    /// Write `bytes` to `path` (create/truncate) and `fsync` before
    /// returning, then drop the handle explicitly. Plain `fs::write` alone
    /// was observed, under heavy concurrent filesystem activity in this
    /// crate's own test suite, to sometimes return success for a write that
    /// a same-process `fs::read_to_string` moments later did not yet see;
    /// an explicit `sync_all` closes that window far more reliably than the
    /// bare convenience function does.
    fn write_and_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = fs::File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        Ok(())
    }

    // -- clients --------------------------------------------------------

    /// Return the stored registration record for `client_id`, or `None`.
    pub fn get_client(&self, client_id: &str) -> Option<Value> {
        self.read().clients.get(client_id).cloned()
    }

    /// Insert or replace a client registration record.
    pub fn put_client(&self, client_id: &str, record: Value) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        state.clients.insert(client_id.to_string(), record);
        self.write(&state);
    }

    /// Summarise registered clients with live token counts (for
    /// `remind_me_revoke_clients`).
    pub fn list_clients(&self) -> Vec<OAuthClientSummary> {
        let state = self.read();
        state
            .clients
            .iter()
            .map(|(client_id, record)| {
                let count = |kind: TokenKind| {
                    state
                        .tokens(kind)
                        .values()
                        .filter(|meta| {
                            meta.get("client_id").and_then(Value::as_str) == Some(client_id)
                        })
                        .count()
                };
                OAuthClientSummary {
                    client_id: client_id.clone(),
                    client_name: record
                        .get("client_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    client_id_issued_at: record.get("client_id_issued_at").and_then(Value::as_i64),
                    redirect_uris: record.get("redirect_uris").and_then(Value::as_array).map(
                        |arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        },
                    ),
                    access_tokens: count(TokenKind::Access),
                    refresh_tokens: count(TokenKind::Refresh),
                }
            })
            .collect()
    }

    /// Delete a client registration and every token it holds.
    ///
    /// Returns `None` when `client_id` is unknown — this is the
    /// "revoke one, by id" operation; there is no "revoke all" (see this
    /// module's tests and `remind_me_mcp`'s `remind_me_revoke_clients` tool
    /// doc for why an empty `client_id` there means *list*, not *revoke
    /// everything*).
    pub fn revoke_client(&self, client_id: &str) -> Option<OAuthRevokeSummary> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        let record = state.clients.remove(client_id)?;
        let counts = state.drop_tokens(client_id);
        self.write(&state);
        Some(OAuthRevokeSummary {
            client_id: client_id.to_string(),
            client_name: record
                .get("client_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            access_tokens: counts.access_tokens,
            refresh_tokens: counts.refresh_tokens,
        })
    }

    // -- tokens -----------------------------------------------------------

    /// Store `token`'s hash (never the raw secret) under `kind`.
    pub fn put_token(&self, kind: TokenKind, token: &str, meta: Value) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        state.tokens_mut(kind).insert(hash_token(token), meta);
        self.write(&state);
    }

    /// Look up a raw token by hash; `None` when unknown.
    pub fn get_token(&self, kind: TokenKind, token: &str) -> Option<Value> {
        self.read().tokens(kind).get(&hash_token(token)).cloned()
    }

    /// Forget a raw token (no-op when unknown).
    pub fn delete_token(&self, kind: TokenKind, token: &str) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        let hash = hash_token(token);
        if state.tokens_mut(kind).remove(&hash).is_some() {
            self.write(&state);
        }
    }

    /// Drop every access/refresh token of `client_id`, keeping the
    /// registration — what RFC 7009 revocation of any one token escalates
    /// to (the reference's `revoke_token`).
    pub fn delete_tokens_for_client(&self, client_id: &str) -> OAuthTokenCounts {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        let counts = state.drop_tokens(client_id);
        if counts.access_tokens > 0 || counts.refresh_tokens > 0 {
            self.write(&state);
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env vars are process-global; serialize this module's tests so they
    // don't race each other the way `cargo test` otherwise would.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        std::env::remove_var(REMOTE_ENABLED_ENV);
        std::env::remove_var(REMOTE_HOST_ENV);
        std::env::remove_var(REMOTE_PORT_ENV);
        std::env::remove_var(REMOTE_TOKEN_ENV);
        std::env::remove_var(REMOTE_TOKEN_FILE_ENV);
        std::env::remove_var(REMOTE_ISSUER_ENV);
        std::env::remove_var(REMOTE_OAUTH_STATE_FILE_ENV);
    }

    #[test]
    fn remote_config_defaults_to_loopback_and_the_reference_port() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = RemoteConfig::from_env();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8768);
        clear_env();
    }

    #[test]
    fn remote_config_reads_host_and_port_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(REMOTE_HOST_ENV, "0.0.0.0");
        std::env::set_var(REMOTE_PORT_ENV, "9999");
        let config = RemoteConfig::from_env();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 9999);
        clear_env();
    }

    #[test]
    fn remote_enabled_is_false_by_default_and_true_for_recognised_truthy_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(!remote_enabled());
        for value in ["1", "true", "TRUE", "yes"] {
            std::env::set_var(REMOTE_ENABLED_ENV, value);
            assert!(remote_enabled(), "{value:?} should enable the connector");
        }
        std::env::set_var(REMOTE_ENABLED_ENV, "0");
        assert!(!remote_enabled());
        clear_env();
    }

    #[test]
    fn resolve_connector_token_prefers_the_env_var_over_a_persisted_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("rrm_remote_token_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("connector_token");
        fs::write(&file, "file-token\n").unwrap();
        std::env::set_var(REMOTE_TOKEN_FILE_ENV, &file);
        std::env::set_var(REMOTE_TOKEN_ENV, "env-token");

        assert_eq!(resolve_connector_token(), "env-token");

        std::env::remove_var(REMOTE_TOKEN_ENV);
        assert_eq!(resolve_connector_token(), "file-token");

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    #[test]
    fn resolve_connector_token_generates_and_persists_on_first_use() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("rrm_remote_token_gen_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("nested").join("connector_token");
        std::env::set_var(REMOTE_TOKEN_FILE_ENV, &file);

        assert!(!file.is_file());
        let first = resolve_connector_token();
        assert!(!first.is_empty());
        assert!(file.is_file());

        // Second resolution reuses the persisted token rather than
        // generating a fresh one every call.
        let second = resolve_connector_token();
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    #[test]
    fn remote_status_reports_token_configured_once_a_token_exists_on_disk() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("rrm_remote_status_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("connector_token");
        std::env::set_var(REMOTE_TOKEN_FILE_ENV, &file);
        std::env::set_var(REMOTE_OAUTH_STATE_FILE_ENV, dir.join("oauth.json"));

        let status = remote_status();
        assert!(!status.enabled);
        assert!(!status.token_configured);
        assert!(!status.oauth_enabled);
        assert!(status.issuer.is_none());

        fs::write(&file, "a-token\n").unwrap();
        let status = remote_status();
        assert!(status.token_configured);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    #[test]
    fn remote_config_reads_the_issuer_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(RemoteConfig::from_env().issuer, None);

        std::env::set_var(REMOTE_ISSUER_ENV, "  https://machine.tailnet.ts.net  ");
        assert_eq!(
            RemoteConfig::from_env().issuer,
            Some("https://machine.tailnet.ts.net".to_string())
        );

        // Blank (whitespace-only) is treated the same as unset.
        std::env::set_var(REMOTE_ISSUER_ENV, "   ");
        assert_eq!(RemoteConfig::from_env().issuer, None);
        clear_env();
    }

    #[test]
    fn remote_status_reports_oauth_enabled_and_the_client_count_once_an_issuer_and_clients_exist() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir =
            std::env::temp_dir().join(format!("rrm_remote_status_oauth_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("oauth.json");
        std::env::set_var(REMOTE_TOKEN_FILE_ENV, dir.join("connector_token"));
        std::env::set_var(REMOTE_OAUTH_STATE_FILE_ENV, &state_file);
        std::env::set_var(REMOTE_ISSUER_ENV, "https://machine.tailnet.ts.net");

        let status = remote_status();
        assert!(status.oauth_enabled);
        assert_eq!(
            status.issuer.as_deref(),
            Some("https://machine.tailnet.ts.net")
        );
        assert_eq!(status.oauth_state_file, state_file.to_string_lossy());
        assert_eq!(status.oauth_clients, 0);

        OAuthStateStore::new(&state_file)
            .put_client("client-1", serde_json::json!({"client_name": "claude.ai"}));
        assert_eq!(remote_status().oauth_clients, 1);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    // -- OAuthStateStore --------------------------------------------------

    fn temp_state_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rrm_oauth_store_{label}_{}_{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        dir.join("oauth.json")
    }

    #[test]
    fn oauth_store_tolerates_a_missing_or_corrupt_state_file() {
        let path = temp_state_path("missing");
        let store = OAuthStateStore::new(&path);
        assert_eq!(store.list_clients(), Vec::new());
        assert!(store.get_client("x").is_none());
        assert!(store.revoke_client("x").is_none());

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert_eq!(store.list_clients(), Vec::new());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_round_trips_a_client_registration() {
        let path = temp_state_path("client");
        let store = OAuthStateStore::new(&path);
        let record = serde_json::json!({
            "client_name": "claude.ai",
            "client_id_issued_at": 1_700_000_000i64,
            "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
        });
        store.put_client("client-1", record.clone());

        assert_eq!(store.get_client("client-1"), Some(record));
        let clients = store.list_clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].client_id, "client-1");
        assert_eq!(clients[0].client_name.as_deref(), Some("claude.ai"));
        assert_eq!(clients[0].access_tokens, 0);
        assert_eq!(clients[0].refresh_tokens, 0);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_state_file_has_0600_permissions() {
        let path = temp_state_path("perms");
        let store = OAuthStateStore::new(&path);
        store.put_client("c1", serde_json::json!({}));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_never_persists_raw_token_values() {
        let path = temp_state_path("hash");
        let store = OAuthStateStore::new(&path);
        store.put_token(
            TokenKind::Access,
            "super-secret-raw-access-token",
            serde_json::json!({ "client_id": "c1" }),
        );

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("super-secret-raw-access-token"));
        assert!(store
            .get_token(TokenKind::Access, "super-secret-raw-access-token")
            .is_some());
        assert!(store.get_token(TokenKind::Access, "wrong-token").is_none());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_delete_tokens_for_client_drops_only_the_targeted_clients_tokens() {
        let path = temp_state_path("delete");
        let store = OAuthStateStore::new(&path);
        store.put_token(
            TokenKind::Access,
            "tok-a",
            serde_json::json!({ "client_id": "c1" }),
        );
        store.put_token(
            TokenKind::Refresh,
            "tok-r",
            serde_json::json!({ "client_id": "c1" }),
        );
        store.put_token(
            TokenKind::Access,
            "tok-b",
            serde_json::json!({ "client_id": "c2" }),
        );

        let counts = store.delete_tokens_for_client("c1");
        assert_eq!(counts.access_tokens, 1);
        assert_eq!(counts.refresh_tokens, 1);
        assert!(store.get_token(TokenKind::Access, "tok-a").is_none());
        assert_eq!(
            store.get_token(TokenKind::Access, "tok-b"),
            Some(serde_json::json!({ "client_id": "c2" }))
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_revoke_client_deletes_the_registration_and_its_tokens_but_leaves_other_clients_alone(
    ) {
        let path = temp_state_path("revoke");
        let store = OAuthStateStore::new(&path);
        store.put_client("c1", serde_json::json!({ "client_name": "claude.ai" }));
        store.put_client("c2", serde_json::json!({ "client_name": "other" }));
        store.put_token(
            TokenKind::Access,
            "tok-a",
            serde_json::json!({ "client_id": "c1" }),
        );
        store.put_token(
            TokenKind::Refresh,
            "tok-r",
            serde_json::json!({ "client_id": "c1" }),
        );

        let summary = store.revoke_client("c1").expect("c1 is registered");
        assert_eq!(summary.client_id, "c1");
        assert_eq!(summary.client_name.as_deref(), Some("claude.ai"));
        assert_eq!(summary.access_tokens, 1);
        assert_eq!(summary.refresh_tokens, 1);

        assert!(store.get_client("c1").is_none());
        assert!(store.get_token(TokenKind::Access, "tok-a").is_none());
        let remaining: Vec<String> = store
            .list_clients()
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(remaining, vec!["c2".to_string()]);

        // Revoking an already-unknown client_id is a no-op that reports None
        // -- not an error, not a silent "revoked everything".
        assert!(store.revoke_client("c1").is_none());
        assert!(store.revoke_client("no-such-client").is_none());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
