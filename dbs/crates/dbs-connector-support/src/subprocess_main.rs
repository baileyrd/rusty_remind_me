//! Generic connector-side implementation of ADR-0001's subprocess
//! protocol (issue #161) — the counterpart to `dbs-core::run_stream`
//! (#157), which implements the host side. A `dbs-connector-<type>`
//! binary's entire `main.rs` is expected to be a few lines: construct
//! its [`dbs_core::Connector`] impl and hand it to [`run_connector_main`].
//!
//! [`run_connector_main`] does both remaining protocol steps in one
//! process lifetime, matching how `dbs-core::registry`'s discovery
//! flow and `dbs-core::run_stream`'s run flow actually spawn a
//! connector (the *same* command/args either way — see
//! `RegisteredConnector`'s doc-comment):
//!
//! 1. **Handshake** (ADR-0001 step 1). Writes one JSON line
//!    self-describing the connector's contract, built entirely from
//!    the [`dbs_core::Connector`] trait's own default-method surface —
//!    nothing here needs to know about a specific connector's fields.
//! 2. **Run** (steps 2-3). Blocks reading one line from stdin. A
//!    discovery-only spawn (`ConnectorRegistry::discover`) never writes
//!    one — it reads the handshake and kills the process — so this
//!    returns cleanly on EOF. A real run
//!    ([`dbs_core::run_connector_subprocess`]) writes a
//!    [`dbs_core::WireRunContext`] line right after spawn; once that
//!    arrives, this applies its `config` map to the connector via
//!    [`dbs_core::Connector::configure`] (ADR-0002 — a no-op for a
//!    connector that declared no override), reconstructs an in-process
//!    [`dbs_core::RunContext`] from the rest of it, and drives
//!    `connector.open`/`fetch`/`close`, streaming each event back as a
//!    [`dbs_core::WireLine`] and finishing with exactly one
//!    [`dbs_core::WireOutcome`] — precisely what
//!    `run_connector_subprocess` reads. A `configure` failure short-
//!    circuits straight to a `WireOutcome::Error`, the same as a
//!    failing `open` does.

use std::cell::RefCell;
use std::io::{BufRead, Write};

use dbs_core::{
    Connector, ConnectorError, ManagedHttpClient, RunContext, Secrets, WireErrorKind, WireLine,
    WireOutcome, WireRunContext,
};

/// Runs the full protocol for `connector`: writes its handshake line,
/// then either returns (no run was requested) or drives a real run and
/// streams its result. See the module doc-comment.
pub fn run_connector_main(connector: &mut dyn Connector) {
    write_handshake(connector);

    let Some(wire_ctx) = read_wire_context() else {
        return;
    };

    if let Err(e) = connector.configure(&wire_ctx.config) {
        write_line(&WireLine::Done(error_outcome(&e)));
        return;
    }

    let ctx = build_run_context(connector, wire_ctx);
    run_and_stream(connector, &ctx);
}

fn write_handshake(connector: &dyn Connector) {
    let handshake = serde_json::json!({
        "type": connector.type_name(),
        "core_api_version": connector.core_api_version(),
        "schema_version": connector.schema_version(),
        "capabilities": connector.capabilities(),
        "secret_keys": connector.secret_keys(),
        "item_kinds": connector
            .item_kinds()
            .iter()
            .map(|k| k.name.as_str())
            .collect::<Vec<_>>(),
        "display_name": non_empty(connector.display_name()),
        "description": non_empty(connector.description()),
        "export_profile": connector.export_profile(),
        "auth_capture": connector.auth_capture(),
        "volatile_fields": connector.volatile_fields(),
        "pip_requirements": connector.pip_requirements(),
        "needs_playwright_browser": connector.needs_playwright_browser(),
    });
    println!("{handshake}");
    let _ = std::io::stdout().flush();
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `None` on a clean EOF (no run requested — the discovery-only case)
/// or an unparseable line (protocol violation on the host's part;
/// nothing useful to report back over a channel the host isn't reading
/// yet, since it hasn't sent a context to run against).
fn read_wire_context() -> Option<WireRunContext> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => serde_json::from_str(line.trim()).ok(),
        Err(_) => None,
    }
}

fn build_run_context(connector: &dyn Connector, wire: WireRunContext) -> RunContext {
    let secrets = Secrets::new(wire.secrets, connector.secret_keys().to_vec());
    let http = connector.wants_managed_http().then(|| {
        RefCell::new(managed_http_client(
            wire.http_timeout,
            wire.http_rate_limit_per_min,
        ))
    });
    RunContext {
        source_id: wire.source_id,
        source_name: wire.source_name,
        cursor: wire.cursor,
        since: wire.since,
        secrets,
        run_id: wire.run_id,
        mode: wire.mode,
        full_refresh: wire.full_refresh,
        limit: wire.limit,
        store_media: wire.store_media,
        max_media_bytes: wire.max_media_bytes,
        download_dir: wire.download_dir,
        items_failed: 0,
        cancel: None,
        http,
    }
}

/// A generous backstop, not a per-format tuned limit — large enough to
/// never affect a real connector response (even a long podcast
/// enclosure download, the largest legitimate response any connector
/// fetches through this client) while still bounding a malicious or
/// misbehaving host's response to *something* short of unbounded
/// memory growth. Only catches a response that honestly declares its
/// size via `Content-Length`; see [`dbs_core::HttpError::TooLarge`]'s
/// doc-comment for that caveat.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Applies `[dbs] http_timeout`/`http_rate_limit_per_min` (carried across
/// the wire in [`WireRunContext`], since the host never makes this
/// connector's own HTTP calls) to the client every connector's real run
/// actually uses. `0.0`/`0` — an older host or a handshake-only spawn's
/// `#[serde(default)]` fallback — leaves `reqwest`'s own defaults in
/// place (no timeout, no rate limit), matching pre-#209 behavior.
fn managed_http_client(http_timeout: f64, http_rate_limit_per_min: u32) -> ManagedHttpClient {
    let mut builder = reqwest::blocking::Client::builder();
    if http_timeout > 0.0 {
        builder = builder.timeout(std::time::Duration::from_secs_f64(http_timeout));
    }
    let client = builder
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());
    let managed = ManagedHttpClient::new(client).max_response_bytes(MAX_RESPONSE_BYTES);
    if http_rate_limit_per_min > 0 {
        managed.rate_limit_per_min(http_rate_limit_per_min)
    } else {
        managed
    }
}

fn run_and_stream(connector: &mut dyn Connector, ctx: &RunContext) {
    if let Err(e) = connector.open(ctx) {
        write_line(&WireLine::Done(error_outcome(&e)));
        return;
    }
    let mut failure: Option<ConnectorError> = None;
    for event in connector.fetch(ctx) {
        match event {
            Ok(ev) => write_line(&WireLine::Event(Box::new(ev))),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    connector.close();
    match failure {
        Some(e) => write_line(&WireLine::Done(error_outcome(&e))),
        None => write_line(&WireLine::Done(WireOutcome::Ok)),
    }
}

fn error_outcome(e: &ConnectorError) -> WireOutcome {
    let (kind, message) = match e {
        ConnectorError::Config(m) => (WireErrorKind::Config, m.clone()),
        ConnectorError::Auth(m) => (WireErrorKind::Auth, m.clone()),
        ConnectorError::Contract(m) => (WireErrorKind::Contract, m.clone()),
        ConnectorError::Transient(m) => (WireErrorKind::Transient, m.clone()),
        ConnectorError::RateLimited(m) => (WireErrorKind::RateLimited, m.clone()),
    };
    WireOutcome::Error { kind, message }
}

fn write_line(line: &WireLine) {
    println!("{}", serde_json::to_string(line).unwrap());
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::{BackupItem, Capabilities, FetchEvent};

    struct OneItemConnector {
        opened: bool,
        closed: bool,
    }

    impl Connector for OneItemConnector {
        fn type_name(&self) -> &str {
            "one_item"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                requires_auth: false,
                ..Capabilities::default()
            }
        }
        fn open(&mut self, _ctx: &RunContext) -> Result<(), ConnectorError> {
            self.opened = true;
            Ok(())
        }
        fn close(&mut self) {
            self.closed = true;
        }
        fn fetch<'a>(
            &'a mut self,
            _ctx: &'a RunContext,
        ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
            assert!(self.opened, "fetch called before open");
            Box::new(std::iter::once(Ok(FetchEvent::Item(
                BackupItem::new("id-1", "item", serde_json::json!({})).unwrap(),
            ))))
        }
    }

    fn wire_ctx() -> WireRunContext {
        WireRunContext {
            source_id: 1,
            source_name: "src".to_string(),
            cursor: None,
            since: None,
            secrets: std::collections::HashMap::new(),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            config: std::collections::HashMap::new(),
            http_timeout: 30.0,
            http_rate_limit_per_min: 0,
        }
    }

    #[test]
    fn build_run_context_carries_wire_fields_through() {
        let connector = OneItemConnector {
            opened: false,
            closed: false,
        };
        let mut wire = wire_ctx();
        wire.mode = "full".to_string();
        wire.limit = Some(7);
        let ctx = build_run_context(&connector, wire);
        assert_eq!(ctx.mode, "full");
        assert_eq!(ctx.limit, Some(7));
        assert_eq!(ctx.source_name, "src");
        assert!(ctx.http.is_none(), "wants_managed_http is false by default");
    }

    struct HttpWantingConnector;
    impl Connector for HttpWantingConnector {
        fn type_name(&self) -> &str {
            "http_wanting"
        }
        fn wants_managed_http(&self) -> bool {
            true
        }
        fn open(&mut self, _ctx: &RunContext) -> Result<(), ConnectorError> {
            Ok(())
        }
        fn fetch<'a>(
            &'a mut self,
            _ctx: &'a RunContext,
        ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
            Box::new(std::iter::empty())
        }
    }

    #[test]
    fn build_run_context_builds_a_managed_http_client_with_configured_timeout_and_rate_limit() {
        let connector = HttpWantingConnector;
        let mut wire = wire_ctx();
        wire.http_timeout = 5.0;
        wire.http_rate_limit_per_min = 3;
        let ctx = build_run_context(&connector, wire);
        assert!(ctx.http.is_some());
    }

    #[test]
    fn build_run_context_builds_a_managed_http_client_with_unset_config() {
        // http_timeout/http_rate_limit_per_min both 0 (the #[serde(default)]
        // fallback for an older host) must not panic — falls back to
        // reqwest's own untimed, unthrottled defaults.
        let connector = HttpWantingConnector;
        let mut wire = wire_ctx();
        wire.http_timeout = 0.0;
        wire.http_rate_limit_per_min = 0;
        let ctx = build_run_context(&connector, wire);
        assert!(ctx.http.is_some());
    }

    #[test]
    fn run_and_stream_opens_before_fetch_and_always_closes() {
        let mut connector = OneItemConnector {
            opened: false,
            closed: false,
        };
        let ctx = build_run_context(&connector, wire_ctx());
        run_and_stream(&mut connector, &ctx);
        assert!(connector.opened);
        assert!(connector.closed);
    }

    struct FailingOpenConnector;
    impl Connector for FailingOpenConnector {
        fn type_name(&self) -> &str {
            "failing_open"
        }
        fn open(&mut self, _ctx: &RunContext) -> Result<(), ConnectorError> {
            Err(ConnectorError::Auth("no token".to_string()))
        }
        fn fetch<'a>(
            &'a mut self,
            _ctx: &'a RunContext,
        ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
            panic!("fetch must not be called when open() fails");
        }
    }

    #[test]
    fn a_failing_open_never_calls_fetch() {
        let mut connector = FailingOpenConnector;
        let ctx = build_run_context(&connector, wire_ctx());
        // Panics (failing the test) if fetch() is ever reached.
        run_and_stream(&mut connector, &ctx);
    }

    #[test]
    fn error_outcome_maps_every_connector_error_variant() {
        let cases = [
            (ConnectorError::Config("a".into()), WireErrorKind::Config),
            (ConnectorError::Auth("a".into()), WireErrorKind::Auth),
            (
                ConnectorError::Contract("a".into()),
                WireErrorKind::Contract,
            ),
            (
                ConnectorError::Transient("a".into()),
                WireErrorKind::Transient,
            ),
            (
                ConnectorError::RateLimited("a".into()),
                WireErrorKind::RateLimited,
            ),
        ];
        for (err, expected_kind) in cases {
            match error_outcome(&err) {
                WireOutcome::Error { kind, message } => {
                    assert_eq!(kind, expected_kind);
                    assert_eq!(message, "a");
                }
                WireOutcome::Ok => panic!("expected an Error outcome"),
            }
        }
    }
}
