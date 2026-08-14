//! Connector capability declarations.
//!
//! Mirrors `src/dbs/core/capabilities.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). Each connector declares a [`Capabilities`] value
//! describing what it can and cannot do; the engine consults these flags
//! rather than probing behavior at run time.

use std::fmt;

/// One entry in a connector's item taxonomy (e.g. `link`, `post`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ItemKind {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
}

/// How a connector's auth artifact can be captured interactively.
///
/// `kind` is `"browser_session"` (a persistent browser-session directory),
/// `"browser_cookies"` (a Netscape `cookies.txt`), or
/// `"browser_storage_state"` (a serialized storage-state JSON). Pure
/// metadata — the capture mechanism itself lives wherever the
/// browser-automation shell-out (round-1 decision: Python/Playwright
/// subprocess) is implemented, not here.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AuthCapture {
    pub kind: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub login_url: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub target_dir_option: String,
    #[serde(default)]
    pub target_path: String,
    /// Whether this capture targets one specific configured source
    /// (`true` — a personal login session, distinct per account) or the
    /// connector type generally (`false`). The shipped web UI (issue
    /// #172) reads this to decide whether a capture/import button calls
    /// a connector-scoped or a source-scoped endpoint.
    #[serde(default)]
    pub per_source: bool,
}

/// Declarative description of a connector's behavior.
///
/// Field-by-field meaning matches the reference exactly — see
/// `src/dbs/core/capabilities.py` for the long-form doc-comments this
/// mirrors.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub supports_incremental: bool,
    pub supports_ordered_cursor: bool,
    pub cursor_kind: String,
    pub supports_full_enumeration: bool,
    pub supports_native_deletes: bool,
    pub produces_media: bool,
    pub media_inline: bool,
    pub items_mutable: bool,
    pub requires_auth: bool,
    pub supports_rate_limit_backoff: bool,
    pub paginated: bool,
    pub concurrency: String,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            supports_incremental: false,
            supports_ordered_cursor: false,
            cursor_kind: "opaque".to_string(),
            supports_full_enumeration: false,
            supports_native_deletes: false,
            produces_media: false,
            media_inline: false,
            items_mutable: true,
            requires_auth: true,
            supports_rate_limit_backoff: false,
            paginated: true,
            concurrency: "parallel".to_string(),
        }
    }
}

/// An internally-contradictory [`Capabilities`] flag combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoherenceError(pub String);

impl fmt::Display for CoherenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CoherenceError {}

impl Capabilities {
    /// Rejects internally contradictory flag combinations, mirroring the
    /// reference's `assert_coherent`.
    pub fn assert_coherent(&self) -> Result<(), CoherenceError> {
        if self.supports_ordered_cursor && !self.supports_incremental {
            return Err(CoherenceError(
                "supports_ordered_cursor=true requires supports_incremental=true".to_string(),
            ));
        }
        if self.media_inline && !self.produces_media {
            return Err(CoherenceError(
                "media_inline=true requires produces_media=true".to_string(),
            ));
        }
        if self.concurrency != "parallel" && self.concurrency != "serial" {
            return Err(CoherenceError(format!(
                "concurrency must be 'parallel' or 'serial', not {:?}",
                self.concurrency
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_coherent() {
        assert!(Capabilities::default().assert_coherent().is_ok());
    }

    #[test]
    fn ordered_cursor_requires_incremental() {
        let caps = Capabilities {
            supports_ordered_cursor: true,
            supports_incremental: false,
            ..Capabilities::default()
        };
        assert!(caps.assert_coherent().is_err());
    }

    #[test]
    fn media_inline_requires_produces_media() {
        let caps = Capabilities {
            media_inline: true,
            produces_media: false,
            ..Capabilities::default()
        };
        assert!(caps.assert_coherent().is_err());
    }

    #[test]
    fn invalid_concurrency_rejected() {
        let caps = Capabilities {
            concurrency: "bogus".to_string(),
            ..Capabilities::default()
        };
        assert!(caps.assert_coherent().is_err());
    }

    #[test]
    fn valid_combination_is_coherent() {
        let caps = Capabilities {
            supports_incremental: true,
            supports_ordered_cursor: true,
            produces_media: true,
            media_inline: true,
            concurrency: "serial".to_string(),
            ..Capabilities::default()
        };
        assert!(caps.assert_coherent().is_ok());
    }
}
