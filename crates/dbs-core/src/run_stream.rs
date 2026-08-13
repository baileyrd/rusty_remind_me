//! The connector run-stream bridge (issue #157) — ADR-0001's steps 2-3.
//!
//! `registry.rs` (issue #45) only implemented steps 1 (handshake) and 4
//! (collision resolution) of the ADR's subprocess protocol, and said so
//! explicitly in its own scope note: writing a `RunContext` and reading
//! a `FetchEvent` stream back was "a separate concern for whichever
//! issue bridges a `RegisteredConnector` to the `Connector` trait's
//! `fetch` signature." This module is that bridge — the Rust,
//! subprocess-shaped counterpart to the reference's in-process
//! `Engine.run_source` (`src/dbs/core/engine.py`, pinned `@6cc6491`).
//!
//! **Wire protocol.** [`WireRunContext`] is what crosses the boundary on
//! the way in (JSON, one line, written to the child's stdin) — a
//! reduced, serializable projection of [`crate::connector::RunContext`]
//! (no `CancelToken`/`ManagedHttpClient`/`Secrets` accessor: those are
//! either host-only concerns or get reconstructed connector-side from
//! plain data). [`WireLine`] is what crosses on the way out (JSON, one
//! line per value, read from the child's stdout): zero or more
//! [`crate::models::FetchEvent`]s, followed by exactly one
//! [`WireOutcome`] reporting how the run ended. A connector process that
//! exits without ever writing a `WireLine::Done` line, or that writes a
//! line neither variant can parse, is treated as a contract violation —
//! [`crate::errors::ConnectorError::Contract`] — the same way the
//! reference treats `fetch()` yielding an event of an unsupported type.
//!
//! **Orchestration** ([`run_connector_subprocess`]) mirrors
//! `Engine.run_source` closely: buffers items and flushes them (via
//! `Storage::upsert_items` + `Storage::save_cursor`) at a batch-size
//! cap or on a `Checkpoint`, collects `ReconcileMarker` scopes and hands
//! them to [`crate::engine::sweep_deletions`] once the run finishes
//! cleanly, classifies the terminal status/error, and — like the
//! reference — skips both the trailing flush and the sweep when the run
//! ends via a contract violation or a connector-reported error (only a
//! clean `Done::Ok` or an engine-side `break` on `--limit`/cancellation
//! reaches that code), so a run interrupted mid-stream never sweeps
//! deletions from a truncated enumeration.
//!
//! **A deliberate improvement over the reference:** cancellation. The
//! reference's `Engine.run_source` polls `ctx.cancel.cancelled()`
//! between fetched items but can only ever stop *reading* an
//! in-process Python generator — it cannot force that generator to stop
//! running. A connector subprocess is a real OS process, so this module
//! can and does call [`std::process::Child::kill`] once the host is done
//! reading its output, actually terminating the connector's work
//! instead of just abandoning interest in it — the abandon-not-kill
//! constraint `dbs-connector-support::watchdog` documents for threads
//! doesn't apply here, since there's a real process handle to act on.
//!
//! [`SubprocessRunner`] is the production [`crate::service::ConnectorRunner`]
//! this module exists to supply, replacing
//! [`crate::service::UnimplementedRunner`] in `dbs-cli`. It resolves a
//! connector's declared `secret_keys` from the process environment (the
//! *only* values that cross the boundary — "a subprocess literally
//! cannot read a secret it wasn't handed on stdin," per ADR-0001) and a
//! source's `store_media`/`max_media_mb`/download directory from
//! [`crate::config::Config`].
//!
//! **Out of scope, left for follow-up work:** this module drives
//! whatever [`crate::registry::RegisteredConnector::command`] already
//! resolves to — it does not itself make any of the 14 built-in
//! `dbs-connector-*` crates into a real subprocess binary that speaks
//! this protocol (none of them have a `main.rs` yet; they're plain
//! libraries exercised by their own in-process unit tests today), and
//! it does not wire real candidate discovery into `dbs-cli` (every
//! `dbs backup` call site still passes `ConnectorRegistry::from_resolved([])`,
//! an always-empty registry). Both are real, separate gaps surfaced
//! while implementing this issue, not silently left for later.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::config::Config;
use crate::engine::{prepare, sweep_deletions};
use crate::errors::{ConnectorError, DbsError};
use crate::models::{Cursor, FetchEvent, RunStatus};
use crate::registry::RegisteredConnector;
use crate::service::{ConnectorRunOutcome, ConnectorRunner};
use crate::storage::{BatchResult, PreparedItem, Storage};

/// Host → connector, one JSON line written to the child's stdin before
/// any output is read. A reduced, serializable projection of
/// [`crate::connector::RunContext`] — see the module doc-comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRunContext {
    pub source_id: i64,
    pub source_name: String,
    pub cursor: Option<Cursor>,
    pub since: Option<DateTime<Utc>>,
    /// Resolved values for exactly the keys this connector declared in
    /// its handshake's `secret_keys` — never more. A key present in the
    /// environment but not declared isn't in this map at all.
    pub secrets: HashMap<String, String>,
    pub run_id: i64,
    /// `"incremental"` | `"reconcile"` | `"full"`.
    pub mode: String,
    pub full_refresh: bool,
    pub limit: Option<u32>,
    pub store_media: bool,
    pub max_media_bytes: u64,
    pub download_dir: Option<PathBuf>,
}

/// How a connector subprocess reports the way its run ended — the
/// terminal line of the protocol, after zero or more `FetchEvent`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WireOutcome {
    Ok,
    Error {
        kind: WireErrorKind,
        message: String,
    },
}

/// Mirrors [`ConnectorError`]'s variants — the wire vocabulary for
/// "which kind of error", since `ConnectorError` itself isn't
/// `Serialize`/`Deserialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorKind {
    Config,
    Auth,
    Contract,
    Transient,
    RateLimited,
}

impl From<(WireErrorKind, String)> for ConnectorError {
    fn from((kind, message): (WireErrorKind, String)) -> Self {
        match kind {
            WireErrorKind::Config => ConnectorError::Config(message),
            WireErrorKind::Auth => ConnectorError::Auth(message),
            WireErrorKind::Contract => ConnectorError::Contract(message),
            WireErrorKind::Transient => ConnectorError::Transient(message),
            WireErrorKind::RateLimited => ConnectorError::RateLimited(message),
        }
    }
}

/// Connector → host, one JSON line per value: either a fetched event or
/// (exactly once, last) the run's terminal outcome. `Event` is boxed —
/// `FetchEvent::Item(BackupItem)` is by far the largest variant of the
/// three, and it's the common case, so boxing keeps every `WireLine`
/// (including the far more numerous `Done`s in practice: one per run
/// vs. one per item) from paying for the biggest variant's size.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case")]
pub enum WireLine {
    Event(Box<FetchEvent>),
    Done(WireOutcome),
}

/// Items are flushed to storage at least this often even without an
/// explicit `Checkpoint`, bounding host-side memory — mirrors the
/// reference's `Engine.batch_max` default.
const BATCH_MAX: usize = 500;

/// Drives one connector subprocess through ADR-0001's run/stream
/// protocol: spawns it, writes `wire_ctx`, reads its `FetchEvent`
/// stream, and persists it via `storage` exactly like the reference's
/// `Engine.run_source` — see the module doc-comment for the exact
/// parity/divergence points (trailing-flush-skipped-on-error,
/// cancellation actually killing the child).
// `committed_any = true` inside the `flush!` macro is genuinely read by
// the *next* `!committed_any` check at each of this function's several
// call sites — the lint can't see across separate macro expansions, so
// it flags the assignment at the very last call on each path as dead.
#[allow(unused_assignments)]
pub fn run_connector_subprocess(
    storage: &mut dyn Storage,
    connector: &RegisteredConnector,
    wire_ctx: WireRunContext,
    sweep_safety_fraction: f64,
    cancel: Option<&CancelToken>,
) -> Result<ConnectorRunOutcome, DbsError> {
    let source_id = wire_ctx.source_id;
    let run_id = wire_ctx.run_id;
    let mode = wire_ctx.mode.clone();
    let limit = wire_ctx.limit;
    let capabilities = connector.handshake.capabilities.clone();
    let volatile_fields = connector.handshake.volatile_fields.clone();
    let valid_kinds = connector.handshake.item_kinds.clone();
    let store_media = wire_ctx.store_media;
    let max_media_bytes = wire_ctx.max_media_bytes;
    let initial_cursor = wire_ctx.cursor.clone();

    let mut child = match Command::new(&connector.command)
        .args(&connector.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(ConnectorRunOutcome {
                status: RunStatus::Failed,
                error: Some(format!("failed to start connector subprocess: {e}")),
                ..ConnectorRunOutcome::default()
            })
        }
    };

    if let Err(e) = write_context(&mut child, &wire_ctx) {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(ConnectorRunOutcome {
            status: RunStatus::Failed,
            error: Some(format!("failed to write run context: {e}")),
            ..ConnectorRunOutcome::default()
        });
    }

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut reader = BufReader::new(stdout);

    // `reader.lines()`/`read_line()` below block the calling thread
    // inside the read syscall — checking `cancel` between successfully-
    // read lines (as a naive port of the reference's per-item poll
    // would) can never fire while the connector has gone quiet (e.g.
    // hung), which is exactly when cancellation matters most. So a
    // cancelled run is detected by a background thread instead, which
    // kills the child directly — that closes its stdout pipe, which is
    // what actually unblocks the read. `child` moves into an
    // `Arc<Mutex<_>>` only for this; `stdin`/`stdout` were already taken
    // above, so the reads and the killer thread never contend over
    // anything but the kill/wait calls themselves.
    let child = std::sync::Arc::new(std::sync::Mutex::new(child));
    let killer = cancel.map(|c| {
        let child = std::sync::Arc::clone(&child);
        let cancel = c.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_thread = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(std::sync::atomic::Ordering::Relaxed) {
                if cancel.cancelled() {
                    let _ = child.lock().unwrap().kill();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        (handle, stop)
    });

    // Every connector spawn — discovery or a real run alike, per
    // ADR-0001 — starts by writing its handshake line (step 1) before
    // anything else. The caller already has that connector's handshake
    // from an earlier discovery call (`connector.handshake`); this
    // fresh spawn writes its own copy first regardless, so it has to be
    // read and discarded here before the `WireLine` stream (steps 2-3)
    // actually begins.
    let mut handshake_line = String::new();
    let handshake_read = reader.read_line(&mut handshake_line);
    if !matches!(handshake_read, Ok(n) if n > 0) {
        if let Some((handle, stop)) = killer {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = handle.join();
        }
        let error = match handshake_read {
            Ok(_) => "connector subprocess exited before writing its handshake line".to_string(),
            Err(e) => format!("failed to read connector handshake line: {e}"),
        };
        let mut child = child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        return Ok(ConnectorRunOutcome {
            status: RunStatus::Failed,
            error: Some(error),
            ..ConnectorRunOutcome::default()
        });
    }

    let mut buffer: Vec<PreparedItem> = Vec::new();
    let mut items_seen: u64 = 0;
    let mut committed_any = false;
    let mut last_cursor = initial_cursor;
    let mut reconcile_scopes: Option<HashMap<String, HashSet<String>>> = None;
    let mut stats = BatchResult::default();
    let mut warnings: Vec<String> = Vec::new();
    let mut hit_limit = false;
    let mut terminal: Option<Result<(), ConnectorError>> = None;

    macro_rules! flush {
        ($cursor:expr) => {{
            let result =
                storage.upsert_items(source_id, run_id, &buffer, store_media, max_media_bytes)?;
            storage.save_cursor(source_id, $cursor, result.max_updated_at.as_deref(), run_id)?;
            stats.merge(&result);
            committed_any = true;
            buffer.clear();
        }};
    }

    'read: for line in reader.lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(e) => {
                terminal = Some(Err(ConnectorError::Contract(format!(
                    "failed to read connector stdout: {e}"
                ))));
                break;
            }
        };
        let parsed: Result<WireLine, _> = serde_json::from_str(&line);
        match parsed {
            Ok(WireLine::Event(event)) => match *event {
                FetchEvent::Item(item) => {
                    if let Some(limit) = limit {
                        if items_seen >= limit as u64 {
                            warnings.push(format!(
                                "stopped after {items_seen} item(s) (--limit {limit}); \
                                 deletion detection skipped"
                            ));
                            reconcile_scopes = None;
                            hit_limit = true;
                            break 'read;
                        }
                    }
                    items_seen += 1;
                    match prepare(&item, &capabilities, &volatile_fields, &valid_kinds) {
                        Ok(prepared) => {
                            buffer.push(prepared);
                            if buffer.len() >= BATCH_MAX {
                                flush!(last_cursor.as_ref());
                            }
                        }
                        Err(e) => {
                            terminal = Some(Err(e));
                            break 'read;
                        }
                    }
                }
                FetchEvent::Checkpoint(cp) => {
                    last_cursor = Some(cp.cursor.clone());
                    flush!(last_cursor.as_ref());
                }
                FetchEvent::ReconcileMarker(marker) => {
                    reconcile_scopes
                        .get_or_insert_with(HashMap::new)
                        .entry(marker.scope.clone())
                        .or_default()
                        .extend(marker.live_ids.clone());
                }
            },
            Ok(WireLine::Done(WireOutcome::Ok)) => {
                terminal = Some(Ok(()));
                break 'read;
            }
            Ok(WireLine::Done(WireOutcome::Error { kind, message })) => {
                terminal = Some(Err((kind, message).into()));
                break 'read;
            }
            Err(e) => {
                terminal = Some(Err(ConnectorError::Contract(format!(
                    "malformed line from connector subprocess: {e}"
                ))));
                break 'read;
            }
        }
    }

    // Stop the killer thread's poll loop (a no-op if it already fired
    // and returned) and join it before touching `child` again, so it
    // can never race the `kill`/`wait` below.
    if let Some((handle, stop)) = killer {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = handle.join();
    }
    let cancelled = cancel.is_some_and(|c| c.cancelled());

    // A subprocess is a real process, unlike an abandoned watchdog
    // thread — always try to actually stop it once the host is done
    // reading, whatever the reason. Harmless (and ignored) if it has
    // already exited on its own (including via the killer thread above).
    {
        let mut child = child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    if cancelled {
        reconcile_scopes = None;
    }

    let (status, error): (RunStatus, Option<String>) = match terminal {
        None if cancelled => {
            if !buffer.is_empty() || !committed_any {
                flush!(last_cursor.as_ref());
            }
            warnings.push(
                "manually stopped before completion — committed data and the cursor \
                 are preserved; the next run resumes from the last checkpoint"
                    .to_string(),
            );
            (RunStatus::Interrupted, None)
        }
        None if hit_limit => {
            // Reached `--limit` mid-stream: engine-enforced early stop,
            // not a connector error — finish like a clean run.
            if !buffer.is_empty() || !committed_any {
                flush!(last_cursor.as_ref());
            }
            (RunStatus::Success, None)
        }
        None => {
            // The subprocess exited (EOF on stdout) without ever writing
            // a terminal `WireLine::Done` line — a protocol violation,
            // same bucket as an unparseable line.
            let status = if committed_any {
                RunStatus::Partial
            } else {
                RunStatus::Failed
            };
            (
                status,
                Some(
                    "connector subprocess exited without reporting a terminal outcome \
                     (protocol violation)"
                        .to_string(),
                ),
            )
        }
        Some(Ok(())) => {
            if !buffer.is_empty() || !committed_any {
                flush!(last_cursor.as_ref());
            }
            if let Some(scopes) = reconcile_scopes {
                if matches!(mode.as_str(), "full" | "reconcile")
                    && capabilities.supports_full_enumeration
                {
                    let outcome = sweep_deletions(
                        storage,
                        source_id,
                        run_id,
                        &scopes,
                        sweep_safety_fraction,
                    )?;
                    stats.deleted += outcome.deleted;
                    stats.revisions += outcome.revisions;
                    warnings.extend(outcome.warnings);
                }
            }
            (RunStatus::Success, None)
        }
        Some(Err(e)) => {
            // Mirrors the reference: an exception mid-stream skips both
            // the trailing flush and the sweep entirely — only what was
            // already committed via a prior batch/checkpoint flush
            // survives; the buffered tail since then is re-fetched next
            // run instead.
            let status = if committed_any {
                RunStatus::Partial
            } else {
                RunStatus::Failed
            };
            (status, Some(e.to_string()))
        }
    };

    if items_seen == 0 && !cancelled && error.is_none() {
        warnings.push(
            "run enumerated 0 items — if this source should not be empty, check its \
             auth/config"
                .to_string(),
        );
    }

    Ok(ConnectorRunOutcome {
        status,
        stats,
        items_seen,
        cursor_after: last_cursor,
        error,
        warnings,
    })
}

fn write_context(child: &mut Child, wire_ctx: &WireRunContext) -> std::io::Result<()> {
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let line = serde_json::to_string(wire_ctx)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writeln!(stdin, "{line}")?;
    stdin.flush()?;
    // Drop stdin (closing it) so a connector reading to EOF sees the
    // context is complete — same convention as a single-shot request.
    drop(stdin);
    Ok(())
}

/// Production [`ConnectorRunner`]: resolves a connector's declared
/// secrets from the process environment and a source's media/download
/// settings from `config`, then drives [`run_connector_subprocess`].
/// See the module doc-comment for what this doesn't yet cover.
pub struct SubprocessRunner<'a> {
    config: &'a Config,
}

impl<'a> SubprocessRunner<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }
}

impl ConnectorRunner for SubprocessRunner<'_> {
    #[allow(clippy::too_many_arguments)]
    fn run_connector(
        &self,
        storage: &mut dyn Storage,
        connector: &RegisteredConnector,
        run_id: i64,
        source_id: i64,
        source_name: &str,
        mode: &str,
        cursor: Option<&Cursor>,
        since: Option<DateTime<Utc>>,
        limit: Option<u32>,
        cancel: Option<&CancelToken>,
    ) -> Result<ConnectorRunOutcome, DbsError> {
        let secrets: HashMap<String, String> = connector
            .handshake
            .secret_keys
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|v| (key.clone(), v)))
            .collect();
        let sc = self.config.sources.get(source_name);
        let store_media = sc.is_some_and(|s| s.store_media);
        let max_media_bytes = sc.map(|s| s.max_media_mb as u64 * 1024 * 1024).unwrap_or(0);
        let download_dir = Some(self.config.download_dir_for(source_name));

        let wire_ctx = WireRunContext {
            source_id,
            source_name: source_name.to_string(),
            cursor: cursor.cloned(),
            since,
            secrets,
            run_id,
            mode: mode.to_string(),
            full_refresh: mode == "full",
            limit,
            store_media,
            max_media_bytes,
            download_dir,
        };

        run_connector_subprocess(
            storage,
            connector,
            wire_ctx,
            self.config.sweep_safety_fraction,
            cancel,
        )
    }
}
