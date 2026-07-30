//! Remote MCP connector configuration and token resolution — the FT-05
//! (secret-path/bearer, no OAuth) slice of `#57`.
//!
//! The Streamable HTTP transport itself lives in the separate
//! `remind_me_remote` crate (tokio/axum/rmcp), not here. This module stays
//! in `remind_me_core` deliberately: `remind_me_mcp`'s
//! `remind_me_server_status` tool needs to report whether the connector is
//! configured, but must not pull tokio/axum/rmcp into a crate that is
//! otherwise entirely synchronous just to read an env var and stat a file.
//! Splitting "state a sync caller can report" from "the async server that
//! serves it" mirrors [`crate::webhook::WebhookStatus`] and
//! [`crate::sync::worker::SyncWorkerStatus`], which draw the same line
//! between their status structs (here, sync, dependency-light) and their
//! actual listener threads (also here, but could just as easily not be).
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

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
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
        Self { host, port }
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

fn generate_token() -> String {
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
/// shape: plain, `Serialize`-able, gathered by a sync free function. The
/// reference's `get_remote_status()` also reports OAuth fields
/// (`oauth_enabled`, `issuer`, `oauth_clients`, ...) — omitted here since
/// FT-07/OAuth (tracked separately as `#86`) is out of scope for this slice
/// and those fields could otherwise never be anything but "disabled".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub token_file: String,
    pub token_configured: bool,
}

/// Gather [`RemoteStatus`]. Reads env vars and stats the token file at call
/// time (not cached) so tests can monkeypatch them, same as
/// [`RemoteConfig::from_env`].
pub fn remote_status() -> RemoteStatus {
    let config = RemoteConfig::from_env();
    let token_file = token_file_path();
    let token_configured = std::env::var(REMOTE_TOKEN_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || token_file.is_file();
    RemoteStatus {
        enabled: remote_enabled(),
        host: config.host,
        port: config.port,
        token_file: token_file.to_string_lossy().to_string(),
        token_configured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; serialize this module's tests so they
    // don't race each other the way `cargo test` otherwise would.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        std::env::remove_var(REMOTE_ENABLED_ENV);
        std::env::remove_var(REMOTE_HOST_ENV);
        std::env::remove_var(REMOTE_PORT_ENV);
        std::env::remove_var(REMOTE_TOKEN_ENV);
        std::env::remove_var(REMOTE_TOKEN_FILE_ENV);
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

        let status = remote_status();
        assert!(!status.enabled);
        assert!(!status.token_configured);

        fs::write(&file, "a-token\n").unwrap();
        let status = remote_status();
        assert!(status.token_configured);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }
}
