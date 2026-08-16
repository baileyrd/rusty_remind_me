//! Connector contract version gating.
//!
//! Mirrors `src/dbs/core/versioning.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). A connector declares the `core_api_version` it was
//! written against; the registry (ADR-0001) refuses to load a connector
//! whose declared version is incompatible, instead of letting it fail deep
//! inside a fetch.
//!
//! Compatibility rule (semver-ish, single integer for v1): a connector is
//! compatible iff it declares the *same* version as the core. When the
//! core contract grows in a backward-compatible way the number stays; a
//! breaking change bumps it.
//!
//! This formalizes the placeholder `CORE_API_VERSION` that issue #4
//! defined directly in `connector` — it's re-exported from here now,
//! matching the reference's `dbs.CORE_API_VERSION` / `versioning.
//! CURRENT_API_VERSION` split (two names, one value).

/// The core API version this crate implements against.
pub const CORE_API_VERSION: u32 = 1;

/// Alias for [`CORE_API_VERSION`], matching the reference's
/// `versioning.CURRENT_API_VERSION` name.
pub const CURRENT_API_VERSION: u32 = CORE_API_VERSION;

/// True if a connector built against `connector_version` may load.
pub fn is_api_compatible(connector_version: u32) -> bool {
    connector_version == CURRENT_API_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_version_is_compatible() {
        assert!(is_api_compatible(CURRENT_API_VERSION));
    }

    #[test]
    fn different_version_is_incompatible() {
        assert!(!is_api_compatible(CURRENT_API_VERSION + 1));
        assert!(!is_api_compatible(0));
    }

    #[test]
    fn current_api_version_aliases_core_api_version() {
        assert_eq!(CURRENT_API_VERSION, CORE_API_VERSION);
    }
}
