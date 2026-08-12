//! Per-source export rules: what to export, and how it renders.
//!
//! Mirrors `src/dbs/core/export_profile.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). A connector knows things about its own items that a
//! generic exporter cannot infer — Reddit's natural grouping axis is the
//! subreddit, YouTube's is the channel, Raindrop's is the user's own tags.
//! [`ExportProfile`] fixes that by naming the real fields; paths resolve
//! against an item's verbatim `raw` payload, so a profile works for any
//! connector without either side changing code.
//!
//! Two independent concerns, deliberately in one type because both are
//! "what this source does at export time":
//!
//! * **Selection** (`enabled`, `item_kinds`) — applies to every export
//!   format, not just the wiki exporter.
//! * **Rendering** (`group_by`, `body_from`, `page_per`) — the wiki
//!   exporter only; other formats ignore these fields.
//!
//! Resolution order is connector default, then the
//! `[sources.NAME.export]` config block, field by field
//! ([`resolve_export_profile`]).
//!
//! **This issue was missed entirely in the original gap-analysis pass**
//! (same failure class as `BackupService`/`service.py`) — `connector.rs`
//! and `config.rs` both previously had no way to declare or override an
//! export profile at all; this issue adds `Connector::export_profile`
//! and `SourceConfig::export`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::DbsError;
use crate::storage::ItemRow;

/// Valid values for [`ExportProfile::page_per`].
pub const PAGE_PER: &[&str] = &["topic", "item"];

/// Resolved export rules for one source.
///
/// Serializable so a connector subprocess can declare its default
/// profile in its ADR-0001 handshake (`registry::Handshake::
/// export_profile`) — `#[serde(default)]` per field mirrors the
/// reference's pydantic field defaults, so a handshake can omit fields
/// it doesn't care to override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportProfile {
    /// Include this source's items in exports at all (every format).
    pub enabled: bool,
    /// Restrict to these item kinds; `None` means every kind.
    pub item_kinds: Option<Vec<String>>,
    /// Raw field paths that become wiki hub pages, e.g. `["subreddit"]`.
    /// A field holding a list yields one page per element. Empty falls
    /// back to grouping on `tags`.
    pub group_by: Vec<String>,
    /// Raw field paths to use as the wiki page body; first non-empty
    /// wins. Empty falls back to the item's own `body` column.
    pub body_from: Vec<String>,
    /// Overrides the export's grouping for this source: `"topic"` or
    /// `"item"` — must be one of [`PAGE_PER`] when set.
    pub page_per: Option<String>,
}

impl Default for ExportProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            item_kinds: None,
            group_by: Vec::new(),
            body_from: Vec::new(),
            page_per: None,
        }
    }
}

impl ExportProfile {
    pub fn accepts_kind(&self, item_kind: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        match &self.item_kinds {
            None => true,
            Some(kinds) => item_kind.is_some_and(|k| kinds.iter().any(|ik| ik == k)),
        }
    }
}

/// The `[sources.NAME.export]` config block.
///
/// Every field is optional and defaults to `None`, meaning "not set" —
/// that's what lets an override leave a connector's default in place
/// instead of silently resetting it to [`ExportProfile`]'s own default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportProfileOverride {
    pub enabled: Option<bool>,
    pub item_kinds: Option<Vec<String>>,
    pub group_by: Option<Vec<String>>,
    pub body_from: Option<Vec<String>>,
    pub page_per: Option<String>,
}

/// Merges a connector's declared default with a source's config block,
/// field by field: a field the config didn't set keeps the connector's
/// value. Returns a plain default profile when neither side declares
/// one. Errors if the resolved `page_per` isn't in [`PAGE_PER`].
pub fn resolve_export_profile(
    default: Option<&ExportProfile>,
    r#override: Option<&ExportProfileOverride>,
) -> Result<ExportProfile, DbsError> {
    let mut profile = default.cloned().unwrap_or_default();
    if let Some(o) = r#override {
        if let Some(enabled) = o.enabled {
            profile.enabled = enabled;
        }
        if let Some(item_kinds) = &o.item_kinds {
            profile.item_kinds = Some(item_kinds.clone());
        }
        if let Some(group_by) = &o.group_by {
            profile.group_by = group_by.clone();
        }
        if let Some(body_from) = &o.body_from {
            profile.body_from = body_from.clone();
        }
        if let Some(page_per) = &o.page_per {
            profile.page_per = Some(page_per.clone());
        }
    }
    if let Some(page_per) = &profile.page_per {
        if !PAGE_PER.contains(&page_per.as_str()) {
            return Err(DbsError::Config(format!(
                "invalid export page_per {page_per:?}. Available: {PAGE_PER:?}"
            )));
        }
    }
    Ok(profile)
}

/// Resolves a dotted field path against an export row.
///
/// `raw` is searched first (connector-specific fields like `subreddit`
/// live there), falling back to the row's own normalized columns so a
/// profile can also name `item_kind`/`url`/`title` directly. `None` when
/// the path doesn't resolve, or when `raw` was omitted (an export run
/// with `--no-raw`).
pub fn raw_value(row: &ItemRow, path: &str) -> Option<Value> {
    if let Some(raw) = row.get("raw") {
        if raw.is_object() {
            if let Some(found) = traverse(raw, path) {
                return Some(found);
            }
        }
    }
    let row_value = Value::Object(row.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    traverse(&row_value, path)
}

fn traverse(value: &Value, path: &str) -> Option<Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.as_object()?.get(part)?;
    }
    Some(current.clone())
}

/// The values `path` contributes for one row, as page-ready strings.
///
/// A scalar yields one value, a list yields one per element (a raw
/// field such as Raindrop's `tags` is genuinely multi-valued), and
/// anything empty, boolean, or non-scalar is dropped rather than
/// producing a page titled after an object.
pub fn group_values(row: &ItemRow, path: &str) -> Vec<String> {
    let Some(value) = raw_value(row, path) else {
        return Vec::new();
    };
    if value.is_boolean() || value.is_null() {
        return Vec::new();
    }
    let items: Vec<Value> = match value {
        Value::Array(a) => a,
        other => vec![other],
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            Value::String(s) => {
                let trimmed = s.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .collect()
}

/// Human page-title prefix for a `group_by` path (`"channel"` ->
/// `"Channel"`, `"channel_name"` -> `"Channel Name"`).
pub fn axis_label(path: &str) -> String {
    let leaf = path.rsplit('.').next().unwrap_or(path);
    let spaced = leaf.replace('_', " ");
    let trimmed = spaced.trim();
    if trimmed.is_empty() {
        return "Group".to_string();
    }
    trimmed
        .split_whitespace()
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_case_word(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn row(raw: Value) -> ItemRow {
        let mut r = HashMap::new();
        r.insert("raw".to_string(), raw);
        r
    }

    #[test]
    fn default_profile_is_enabled_with_no_restrictions() {
        let p = ExportProfile::default();
        assert!(p.enabled);
        assert!(p.item_kinds.is_none());
        assert!(p.accepts_kind(Some("post")));
        assert!(p.accepts_kind(None));
    }

    #[test]
    fn accepts_kind_is_false_when_disabled() {
        let p = ExportProfile {
            enabled: false,
            ..Default::default()
        };
        assert!(!p.accepts_kind(Some("post")));
    }

    #[test]
    fn accepts_kind_restricts_to_declared_kinds() {
        let p = ExportProfile {
            item_kinds: Some(vec!["post".to_string()]),
            ..Default::default()
        };
        assert!(p.accepts_kind(Some("post")));
        assert!(!p.accepts_kind(Some("comment")));
        assert!(!p.accepts_kind(None));
    }

    #[test]
    fn resolve_with_no_default_or_override_returns_the_plain_default() {
        let profile = resolve_export_profile(None, None).unwrap();
        assert_eq!(profile, ExportProfile::default());
    }

    #[test]
    fn resolve_falls_back_to_the_connector_default_with_no_override() {
        let default = ExportProfile {
            group_by: vec!["subreddit".to_string()],
            ..Default::default()
        };
        let profile = resolve_export_profile(Some(&default), None).unwrap();
        assert_eq!(profile.group_by, vec!["subreddit".to_string()]);
    }

    #[test]
    fn resolve_override_narrows_item_kinds_field_by_field() {
        let default = ExportProfile {
            group_by: vec!["subreddit".to_string()],
            body_from: vec!["selftext".to_string()],
            ..Default::default()
        };
        let over = ExportProfileOverride {
            item_kinds: Some(vec!["post".to_string()]),
            ..Default::default()
        };
        let profile = resolve_export_profile(Some(&default), Some(&over)).unwrap();
        // Unset override fields keep the connector's default...
        assert_eq!(profile.group_by, vec!["subreddit".to_string()]);
        assert_eq!(profile.body_from, vec!["selftext".to_string()]);
        // ...but the field the override *did* set wins.
        assert_eq!(profile.item_kinds, Some(vec!["post".to_string()]));
    }

    #[test]
    fn resolve_rejects_an_invalid_page_per() {
        let over = ExportProfileOverride {
            page_per: Some("bogus".to_string()),
            ..Default::default()
        };
        assert!(resolve_export_profile(None, Some(&over)).is_err());
    }

    #[test]
    fn resolve_accepts_a_valid_page_per() {
        let over = ExportProfileOverride {
            page_per: Some("item".to_string()),
            ..Default::default()
        };
        let profile = resolve_export_profile(None, Some(&over)).unwrap();
        assert_eq!(profile.page_per.as_deref(), Some("item"));
    }

    #[test]
    fn raw_value_reads_from_the_raw_payload_first() {
        let r = row(serde_json::json!({"subreddit": "rust"}));
        assert_eq!(raw_value(&r, "subreddit"), Some(Value::from("rust")));
    }

    #[test]
    fn raw_value_falls_back_to_the_normalized_row_columns() {
        let mut r = row(serde_json::json!({}));
        r.insert("item_kind".to_string(), Value::from("post"));
        assert_eq!(raw_value(&r, "item_kind"), Some(Value::from("post")));
    }

    #[test]
    fn raw_value_resolves_dotted_paths() {
        let r = row(serde_json::json!({"author": {"name": "alice"}}));
        assert_eq!(raw_value(&r, "author.name"), Some(Value::from("alice")));
    }

    #[test]
    fn raw_value_is_none_for_a_missing_path() {
        let r = row(serde_json::json!({"subreddit": "rust"}));
        assert!(raw_value(&r, "missing").is_none());
        assert!(raw_value(&r, "subreddit.nested").is_none());
    }

    #[test]
    fn group_values_yields_one_per_list_element() {
        let r = row(serde_json::json!({"tags": ["rust", "async", ""]}));
        assert_eq!(group_values(&r, "tags"), vec!["rust", "async"]);
    }

    #[test]
    fn group_values_yields_a_single_scalar_as_one_value() {
        let r = row(serde_json::json!({"subreddit": "rust"}));
        assert_eq!(group_values(&r, "subreddit"), vec!["rust"]);
    }

    #[test]
    fn group_values_drops_booleans_and_missing_paths() {
        let r = row(serde_json::json!({"flag": true}));
        assert!(group_values(&r, "flag").is_empty());
        assert!(group_values(&r, "missing").is_empty());
    }

    #[test]
    fn group_values_drops_non_scalar_list_elements() {
        let r = row(serde_json::json!({"authors": [{"name": "alice"}, "bob"]}));
        assert_eq!(group_values(&r, "authors"), vec!["bob"]);
    }

    #[test]
    fn axis_label_title_cases_the_last_path_segment() {
        assert_eq!(axis_label("channel"), "Channel");
        assert_eq!(axis_label("channel_name"), "Channel Name");
        assert_eq!(axis_label("author.subreddit"), "Subreddit");
    }

    #[test]
    fn axis_label_falls_back_to_group_when_empty() {
        assert_eq!(axis_label(""), "Group");
        assert_eq!(axis_label("___"), "Group");
    }
}
