//! Error hierarchy for `rusty_dbs`.
//!
//! Mirrors `src/dbs/core/errors.py` in baileyrd/Daily-Backup-System (pinned
//! `@6cc6491`). The reference uses an exception *class* hierarchy so a
//! caller can `except TransientFetchError` and also catch `RateLimitedError`
//! (its subclass). Rust has no subclassing, so the "is this retryable?"
//! relationship is expressed as a classification method
//! ([`ConnectorError::is_retryable`]) instead of nested variant matching —
//! same semantics, idiomatic shape.
//!
//! Connectors signal intent to the engine through [`ConnectorError`]:
//!
//! * [`ConnectorError::Config`] / [`ConnectorError::Auth`] — abort the run
//!   immediately; not retryable.
//! * [`ConnectorError::Transient`] / [`ConnectorError::RateLimited`] —
//!   retryable; the run ends `partial`/`failed` and the next scheduled run
//!   resumes from the last committed cursor.
//! * [`ConnectorError::Contract`] — the connector violated the plugin
//!   contract (a programming error), surfaced loudly.

use std::fmt;

/// A connector targets an incompatible `core_api_version`, or couldn't be
/// found/loaded at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorLoadError {
    NotFound(String),
    IncompatibleVersion {
        plugin: String,
        required: String,
        found: String,
    },
}

impl fmt::Display for ConnectorLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(plugin) => write!(f, "connector plugin not found: {plugin}"),
            Self::IncompatibleVersion {
                plugin,
                required,
                found,
            } => write!(
                f,
                "connector {plugin} targets an incompatible core_api_version: requires {required}, found {found}"
            ),
        }
    }
}

impl std::error::Error for ConnectorLoadError {}

/// Errors raised by/about a connector at run time.
///
/// Not retryable: [`Self::Config`], [`Self::Auth`], [`Self::Contract`].
/// Retryable: [`Self::Transient`], [`Self::RateLimited`] — see
/// [`Self::is_retryable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorError {
    /// The connector's own (validated) configuration is unusable.
    Config(String),
    /// Authentication failed, or a required secret is missing.
    Auth(String),
    /// The connector violated the plugin contract (a programming error).
    Contract(String),
    /// A temporary failure (network blip, 5xx).
    Transient(String),
    /// The upstream API rate-limited us.
    RateLimited(String),
}

impl ConnectorError {
    /// True for [`Self::Transient`] and [`Self::RateLimited`] — the run
    /// should end `partial`/`failed` and resume next time, not abort. This
    /// is the Rust stand-in for the reference's
    /// `except TransientFetchError` catching `RateLimitedError` too.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::RateLimited(_))
    }
}

impl fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "connector config error: {msg}"),
            Self::Auth(msg) => write!(f, "connector auth error: {msg}"),
            Self::Contract(msg) => write!(f, "connector contract violation: {msg}"),
            Self::Transient(msg) => write!(f, "transient fetch error: {msg}"),
            Self::RateLimited(msg) => write!(f, "rate limited: {msg}"),
        }
    }
}

impl std::error::Error for ConnectorError {}

/// A backup run could not be started (e.g. source locked, unknown source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupRunError {
    UnknownSource(String),
    /// Another run already holds the lock for this source.
    SourceLocked(String),
}

impl fmt::Display for BackupRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSource(name) => write!(f, "unknown source: {name}"),
            Self::SourceLocked(name) => write!(f, "source already locked: {name}"),
        }
    }
}

impl std::error::Error for BackupRunError {}

/// Top-level error type for `rusty_dbs`. Base of the reference's `DBSError`
/// hierarchy, flattened into one enum wrapping each family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbsError {
    /// The user's configuration file is invalid.
    Config(String),
    Load(ConnectorLoadError),
    Connector(ConnectorError),
    Run(BackupRunError),
}

impl fmt::Display for DbsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Load(e) => write!(f, "{e}"),
            Self::Connector(e) => write!(f, "{e}"),
            Self::Run(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DbsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(_) => None,
            Self::Load(e) => Some(e),
            Self::Connector(e) => Some(e),
            Self::Run(e) => Some(e),
        }
    }
}

impl From<ConnectorLoadError> for DbsError {
    fn from(e: ConnectorLoadError) -> Self {
        Self::Load(e)
    }
}

impl From<ConnectorError> for DbsError {
    fn from(e: ConnectorError) -> Self {
        Self::Connector(e)
    }
}

impl From<BackupRunError> for DbsError {
    fn from(e: BackupRunError) -> Self {
        Self::Run(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_and_rate_limited_are_retryable() {
        assert!(ConnectorError::Transient("timeout".into()).is_retryable());
        assert!(ConnectorError::RateLimited("429".into()).is_retryable());
    }

    #[test]
    fn config_auth_contract_are_not_retryable() {
        assert!(!ConnectorError::Config("bad toml".into()).is_retryable());
        assert!(!ConnectorError::Auth("missing token".into()).is_retryable());
        assert!(!ConnectorError::Contract("unknown item_kind".into()).is_retryable());
    }

    #[test]
    fn connector_load_error_incompatible_version_displays_both_versions() {
        let err = ConnectorLoadError::IncompatibleVersion {
            plugin: "raindrop".to_string(),
            required: "1.x".to_string(),
            found: "2.0".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("raindrop"));
        assert!(msg.contains("1.x"));
        assert!(msg.contains("2.0"));
    }

    #[test]
    fn dbs_error_source_chains_to_the_wrapped_error() {
        let err = DbsError::from(ConnectorError::RateLimited("429".into()));
        let source = std::error::Error::source(&err).expect("should have a source");
        assert_eq!(source.to_string(), "rate limited: 429");
    }

    #[test]
    fn dbs_error_config_has_no_source() {
        let err = DbsError::Config("missing [sources] table".into());
        assert!(std::error::Error::source(&err).is_none());
    }
}
