//! Content hashing for change detection.
//!
//! Mirrors `src/dbs/core/hashing.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). The hash is computed over a *normalized
//! projection* of an item, never over raw bytes — raw-byte hashing
//! produces revision spam from volatile server fields (timestamps,
//! caches, derived domains) and is non-deterministic across JSON key
//! ordering. Hashing a canonical projection makes change detection stable
//! and order-independent while an item's raw payload still stores the
//! verbatim payload for fidelity.
//!
//! `serde_json::Value`'s `Map` type is a `BTreeMap` by default (this
//! workspace doesn't enable the `preserve_order` feature), so
//! `serde_json::to_string` already serializes object keys in sorted
//! order at every nesting level — [`canonical_json`] doesn't need to sort
//! anything itself, just pick compact-but-deterministic formatting.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Serializes `value` deterministically: sorted keys (via `serde_json`'s
/// default `BTreeMap`-backed `Value::Object`), compact separators, UTF-8
/// as-is (no `\uXXXX` escaping of non-ASCII).
///
/// Infallible: `Value` is already fully JSON-native, unlike the
/// reference's `json.dumps(..., default=str)` which has to handle
/// arbitrary Python objects at the boundary.
pub fn canonical_json(value: &Value) -> String {
    // serde_json's compact `to_string` already matches Python's
    // `separators=(",", ":")` and doesn't escape non-ASCII by default.
    serde_json::to_string(value).expect("serializing a Value never fails")
}

/// Returns the SHA-256 hex digest of the canonical form of `projection`.
pub fn content_hash(projection: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(projection).as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let value = json!({"b": 1, "a": {"d": 2, "c": 3}});
        assert_eq!(canonical_json(&value), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn canonical_json_uses_compact_separators() {
        let value = json!({"a": [1, 2, 3]});
        assert_eq!(canonical_json(&value), r#"{"a":[1,2,3]}"#);
    }

    #[test]
    fn canonical_json_keeps_non_ascii_unescaped() {
        let value = json!({"title": "café"});
        assert_eq!(canonical_json(&value), r#"{"title":"café"}"#);
    }

    #[test]
    fn content_hash_is_order_independent() {
        let a = json!({"title": "hi", "id": 1});
        let b = json!({"id": 1, "title": "hi"});
        assert_eq!(content_hash(&a), content_hash(&b));
    }

    #[test]
    fn content_hash_differs_on_real_content_change() {
        let a = json!({"title": "hi"});
        let b = json!({"title": "bye"});
        assert_ne!(content_hash(&a), content_hash(&b));
    }

    #[test]
    fn content_hash_is_a_64_char_hex_sha256_digest() {
        let digest = content_hash(&json!({"x": 1}));
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
