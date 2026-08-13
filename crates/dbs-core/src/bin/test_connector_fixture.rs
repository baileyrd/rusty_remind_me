//! Test-only fixture: a fake `dbs-connector-*` subprocess for exercising
//! `dbs_core::registry`'s handshake protocol and `dbs_core::run_stream`'s
//! run/stream protocol end-to-end without needing a real connector
//! binary. Not part of the public product — spawned only from
//! `tests/registry_integration.rs` and `tests/run_stream_integration.rs`
//! via `env!("CARGO_BIN_EXE_...")`.
//!
//! Usage: `test_connector_fixture <mode> [type_name]`
//!
//! Handshake modes (`tests/registry_integration.rs`):
//! - `valid` — writes a well-formed handshake for `type_name` (default
//!   `"fixture"`) and exits.
//! - `malformed` — writes a line that isn't JSON.
//! - `bad-version` — writes a handshake with an incompatible
//!   `core_api_version`.
//! - `bad-type` — writes a handshake whose `type` fails the naming rule.
//! - `no-output` — exits immediately without writing anything.
//! - `hang` — sleeps well past any reasonable test timeout before
//!   writing anything.
//!
//! Run/stream modes (`tests/run_stream_integration.rs`), all of which
//! first read and parse one [`dbs_core::WireRunContext`] line from
//! stdin — `usage: test_connector_fixture run <scenario> [args...]`:
//! - `run ok <n>` — writes `n` `Item`s (the first one's `raw` echoes
//!   the received [`WireRunContext`] under `"_wire_ctx"`, so a test can
//!   assert on exactly what the host sent), a `Checkpoint`, then
//!   `Done::Ok`.
//! - `run reconcile` — writes two items (`keep`, `drop`) then a
//!   `ReconcileMarker` whose `live_ids` names only `keep`, then
//!   `Done::Ok`.
//! - `run error <kind> <message>` — writes one `Item` then
//!   `Done::Error { kind, message }`.
//! - `run malformed` — writes one `Item` then a line that isn't JSON.
//! - `run no-terminal` — writes one `Item` then exits without ever
//!   writing a `Done` line.
//! - `run hang` — writes one `Item`, flushes, then sleeps well past any
//!   reasonable test timeout — for asserting a cancelled run actually
//!   kills the child instead of waiting for it.

use std::io::{BufRead, Write};
use std::time::Duration;

use dbs_core::{BackupItem, Checkpoint, Cursor, FetchEvent, ReconcileMarker};
use dbs_core::{WireErrorKind, WireLine, WireOutcome, WireRunContext};

fn write_line(value: &WireLine) {
    println!("{}", serde_json::to_string(value).unwrap());
    let _ = std::io::stdout().flush();
}

fn item(id: &str, raw: serde_json::Value) -> WireLine {
    let mut it = BackupItem::new(id, "item", raw).unwrap();
    it.title = Some(id.to_string());
    WireLine::Event(Box::new(FetchEvent::Item(it)))
}

fn read_wire_context() -> WireRunContext {
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).expect("read stdin");
    serde_json::from_str(line.trim()).expect("parse WireRunContext")
}

fn run_fixture(args: &mut dyn Iterator<Item = String>) {
    let scenario = args.next().unwrap_or_default();
    let ctx = read_wire_context();
    match scenario.as_str() {
        "ok" => {
            let n: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
            for i in 0..n {
                let raw = if i == 0 {
                    serde_json::json!({"_wire_ctx": ctx})
                } else {
                    serde_json::json!({})
                };
                write_line(&item(&format!("item-{i}"), raw));
            }
            write_line(&WireLine::Event(Box::new(FetchEvent::Checkpoint(
                Checkpoint {
                    cursor: Cursor {
                        value: serde_json::json!({"page": 2}),
                    },
                    note: String::new(),
                },
            ))));
            write_line(&WireLine::Done(WireOutcome::Ok));
        }
        "reconcile" => {
            write_line(&item("keep", serde_json::json!({})));
            write_line(&item("drop", serde_json::json!({})));
            write_line(&WireLine::Event(Box::new(FetchEvent::ReconcileMarker(
                ReconcileMarker::new(["keep".to_string()].into_iter().collect()),
            ))));
            write_line(&WireLine::Done(WireOutcome::Ok));
        }
        "error" => {
            let kind = match args.next().as_deref() {
                Some("config") => WireErrorKind::Config,
                Some("auth") => WireErrorKind::Auth,
                Some("contract") => WireErrorKind::Contract,
                Some("rate_limited") => WireErrorKind::RateLimited,
                _ => WireErrorKind::Transient,
            };
            let message = args.next().unwrap_or_else(|| "boom".to_string());
            write_line(&item("item-0", serde_json::json!({})));
            write_line(&WireLine::Done(WireOutcome::Error { kind, message }));
        }
        "malformed" => {
            write_line(&item("item-0", serde_json::json!({})));
            println!("this is not json");
            let _ = std::io::stdout().flush();
        }
        "no-terminal" => {
            write_line(&item("item-0", serde_json::json!({})));
        }
        "hang" => {
            write_line(&item("item-0", serde_json::json!({})));
            std::thread::sleep(Duration::from_secs(3600));
        }
        other => {
            eprintln!("unknown run scenario: {other:?}");
            std::process::exit(2);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();

    if mode == "run" {
        run_fixture(&mut args);
        return;
    }

    let type_name = args.next().unwrap_or_else(|| "fixture".to_string());

    match mode.as_str() {
        "valid" => {
            let handshake = serde_json::json!({
                "type": type_name,
                "core_api_version": 1,
                "schema_version": 1,
                // requires_auth defaults to true (Capabilities::default());
                // explicitly false here since this fixture declares no
                // secret_keys and should pass full contract validation.
                "capabilities": {"requires_auth": false},
                "secret_keys": [],
                "item_kinds": ["item"],
                "display_name": "Fixture",
                "description": "test fixture connector",
            });
            println!("{handshake}");
        }
        "malformed" => {
            println!("this is not json");
        }
        "bad-version" => {
            let handshake = serde_json::json!({
                "type": type_name,
                "core_api_version": 999,
                "schema_version": 1,
                "capabilities": {"requires_auth": false},
                "secret_keys": [],
                "item_kinds": ["item"],
            });
            println!("{handshake}");
        }
        "bad-type" => {
            let handshake = serde_json::json!({
                "type": "Not-Valid",
                "core_api_version": 1,
                "schema_version": 1,
                "capabilities": {},
                "secret_keys": [],
                "item_kinds": ["item"],
            });
            println!("{handshake}");
        }
        "no-output" => {}
        "hang" => {
            std::thread::sleep(Duration::from_secs(3600));
        }
        other => {
            eprintln!("unknown fixture mode: {other:?}");
            std::process::exit(2);
        }
    }
    let _ = std::io::stdout().flush();
}
