//! The connector plugin contract.
//!
//! Mirrors `src/dbs/core/connector.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). A source connector implements [`Connector`] and
//! never touches storage/engine internals directly — it yields a stream of
//! [`FetchEvent`]s from [`Connector::fetch`] and the engine owns all
//! persistence, hashing, revisioning, cursor commits, and deletion logic.
//!
//! `fetch` returns a boxed iterator (rather than an associated type) so
//! `Connector` stays object-safe (`Box<dyn Connector>`) — this is a
//! deliberate head start on issue #5's dynamic-plugin-loading design,
//! which will need trait objects across a `cdylib` boundary. The trait
//! shape here is a first pass; #5's ADR may require revisiting it once
//! the ABI story is settled.
//!
//! [`RunContext`] grows toward the reference's full shape as its
//! dependent pieces land: `secrets` (#6) and `cancel` (#10) are now
//! present; the managed HTTP client (#22) and a logger equivalent are
//! still missing.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::cancel::CancelToken;
use crate::capabilities::{AuthCapture, Capabilities, ItemKind};
use crate::errors::ConnectorError;
use crate::models::{Cursor, FetchEvent};
use crate::secrets::Secrets;
pub use crate::versioning::CORE_API_VERSION;

/// Everything a connector needs for one run, injected by the engine.
///
/// Still missing `http` (#22) and a logger equivalent relative to the
/// reference's full `RunContext`.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub source_id: i64,
    pub source_name: String,
    pub cursor: Option<Cursor>,
    /// Engine watermark = max(updated_at) committed so far.
    pub since: Option<DateTime<Utc>>,
    pub secrets: Secrets,
    pub run_id: i64,
    /// `"incremental"` | `"reconcile"` | `"full"`.
    pub mode: String,
    pub full_refresh: bool,
    pub limit: Option<u32>,
    /// Archive media bytes into the DB (opt-in per source).
    pub store_media: bool,
    /// Per-file size cap in bytes (0 = no cap).
    pub max_media_bytes: u64,
    pub download_dir: Option<PathBuf>,
    /// Connector-reported soft failures for this run (didn't abort the run
    /// but didn't fully succeed either — e.g. a media download to retry
    /// next run).
    pub items_failed: u32,
    /// Cooperative cancellation signal (CLI Ctrl+C / a future web "Stop").
    /// The engine (later issue) polls it between fetched items and halts
    /// gracefully when set. `None` means the run cannot be cancelled.
    pub cancel: Option<CancelToken>,
}

impl RunContext {
    /// Records `n` connector-side soft failures for this run.
    pub fn report_failed(&mut self, n: u32) {
        self.items_failed += n;
    }
}

/// Base contract every source connector implements.
///
/// Default method bodies match the reference's class-level defaults
/// (`Capabilities::default()`, empty tuples, `wants_managed_http = false`,
/// etc.) so a minimal connector only needs to override `type_name` and
/// `fetch`.
pub trait Connector {
    /// Stable machine identifier, e.g. `"raindrop"`. Lowercase
    /// `[a-z][a-z0-9_]*` (not enforced by the type system — validated at
    /// registration time once the registry, issue #5, exists).
    fn type_name(&self) -> &str;

    fn core_api_version(&self) -> u32 {
        CORE_API_VERSION
    }

    /// Bumped by the connector author when the *meaning* of its content
    /// projection changes, so the engine can avoid mass false "updated"s.
    fn schema_version(&self) -> u32 {
        1
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// Names of environment secrets this connector is allowed to read.
    fn secret_keys(&self) -> &[String] {
        &[]
    }

    /// This connector's item taxonomy; every emitted item's `item_kind`
    /// must be one of these names.
    fn item_kinds(&self) -> &[ItemKind] {
        &[]
    }

    /// If true, the engine injects a managed HTTP client (issue #22).
    fn wants_managed_http(&self) -> bool {
        false
    }

    /// Keys stripped from an item's raw payload before computing the
    /// content hash, to avoid revision spam.
    fn volatile_fields(&self) -> &[String] {
        &[]
    }

    fn display_name(&self) -> &str {
        ""
    }

    fn description(&self) -> &str {
        ""
    }

    fn docs_url(&self) -> &str {
        ""
    }

    /// One-line, user-facing guidance for what auth/setup this source
    /// needs.
    fn setup_hint(&self) -> &str {
        ""
    }

    fn pip_requirements(&self) -> &[String] {
        &[]
    }

    fn runtime_imports(&self) -> &[String] {
        &[]
    }

    fn needs_playwright_browser(&self) -> bool {
        false
    }

    /// Declares that this connector's auth artifact can be captured
    /// interactively by a UI tier (round-1 decision: shells out to the
    /// existing Python/Playwright tooling — see `gap-analysis.md`).
    fn auth_capture(&self) -> Option<&AuthCapture> {
        None
    }

    /// This connector's default export rules (issue #49) — `None` means
    /// every emitted item exports under the generic defaults
    /// (`ExportProfile::default()`), with no per-source config override
    /// yet resolved. See `crate::export_profile` for the merge with a
    /// source's `[sources.NAME.export]` config block.
    fn export_profile(&self) -> Option<crate::export_profile::ExportProfile> {
        None
    }

    /// Reports whether the connector's runtime dependencies are ready.
    /// Returns `(ready, hint)`. Default: always ready — override for a
    /// real check (e.g. probing for the `yt-dlp` binary on `PATH`).
    fn check_ready(&self) -> (bool, String) {
        (true, String::new())
    }

    /// Optional: acquire sessions / validate auth eagerly. Default no-op.
    fn open(&mut self, _ctx: &RunContext) -> Result<(), ConnectorError> {
        Ok(())
    }

    /// Always called after `fetch`, even on error. Default no-op.
    fn close(&mut self) {}

    /// Yields a stream of items, checkpoints, and reconcile markers.
    ///
    /// Implementations should read `ctx.cursor`/`ctx.since` to fetch only
    /// what changed, yield a checkpoint at safe commit points (typically
    /// once per upstream page), never mutate the cursor directly, and
    /// return a retryable [`ConnectorError`] (`Transient`/`RateLimited`)
    /// for failures the next scheduled run should recover from. Re-delivery
    /// is safe: the engine's upsert (issue #17) is idempotent by
    /// `(source_id, external_id)` + content hash.
    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BackupItem, Checkpoint};
    use serde_json::json;
    use std::collections::HashSet as StdHashSet;

    /// A minimal connector exercising the trait's default methods plus a
    /// real `fetch` implementation, standing in for the reference doc's
    /// "minimal connector skeleton" example.
    struct FakeConnector {
        emitted_checkpoint: bool,
    }

    impl Connector for FakeConnector {
        fn type_name(&self) -> &str {
            "fake"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_incremental: true,
                ..Capabilities::default()
            }
        }

        fn fetch<'a>(
            &'a mut self,
            _ctx: &'a RunContext,
        ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
            self.emitted_checkpoint = true;
            let item = BackupItem::new("1", "post", json!({"title": "hi"})).unwrap();
            let checkpoint = Checkpoint {
                cursor: Cursor {
                    value: json!({"after": "1"}),
                },
                note: String::new(),
            };
            Box::new(
                vec![
                    Ok(FetchEvent::Item(item)),
                    Ok(FetchEvent::Checkpoint(checkpoint)),
                ]
                .into_iter(),
            )
        }
    }

    fn fake_ctx() -> RunContext {
        RunContext {
            source_id: 1,
            source_name: "fake".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(std::collections::HashMap::new(), Vec::new()),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
        }
    }

    #[test]
    fn defaults_match_the_reference_class_level_defaults() {
        let connector = FakeConnector {
            emitted_checkpoint: false,
        };
        assert_eq!(connector.core_api_version(), CORE_API_VERSION);
        assert_eq!(connector.schema_version(), 1);
        assert!(connector.secret_keys().is_empty());
        assert!(connector.item_kinds().is_empty());
        assert!(!connector.wants_managed_http());
        assert!(connector.auth_capture().is_none());
        assert_eq!(connector.check_ready(), (true, String::new()));
    }

    #[test]
    fn fetch_yields_item_then_checkpoint() {
        let mut connector = FakeConnector {
            emitted_checkpoint: false,
        };
        let ctx = fake_ctx();
        let events: Vec<_> = connector
            .fetch(&ctx)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], FetchEvent::Item(_)));
        assert!(matches!(events[1], FetchEvent::Checkpoint(_)));
        assert!(connector.emitted_checkpoint);
    }

    #[test]
    fn run_context_report_failed_accumulates() {
        let mut ctx = fake_ctx();
        ctx.report_failed(2);
        ctx.report_failed(3);
        assert_eq!(ctx.items_failed, 5);
    }

    #[test]
    fn connector_is_object_safe() {
        // Compiles only if `Connector` is object-safe — the point of
        // returning `Box<dyn Iterator<...>>` from `fetch` instead of an
        // associated type.
        let connector: Box<dyn Connector> = Box::new(FakeConnector {
            emitted_checkpoint: false,
        });
        assert_eq!(connector.type_name(), "fake");
    }

    #[test]
    fn run_context_cancel_field_reflects_the_shared_token() {
        let token = CancelToken::new();
        let mut ctx = fake_ctx();
        ctx.cancel = Some(token.clone());
        assert!(!ctx.cancel.as_ref().unwrap().cancelled());
        token.cancel();
        assert!(ctx.cancel.as_ref().unwrap().cancelled());
    }

    #[test]
    fn reconcile_marker_event_round_trips_through_fetch_event() {
        let marker = crate::models::ReconcileMarker::new(StdHashSet::from(["1".to_string()]));
        let event = FetchEvent::ReconcileMarker(marker.clone());
        match event {
            FetchEvent::ReconcileMarker(m) => assert_eq!(m, marker),
            _ => panic!("expected ReconcileMarker variant"),
        }
    }
}
