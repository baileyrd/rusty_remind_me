//! Test-only fixture: a fake `dbs-connector-*` subprocess for exercising
//! `dbs_core::registry`'s handshake protocol end-to-end without needing a
//! real connector binary. Not part of the public product — spawned only
//! from `tests/registry_integration.rs` via `env!("CARGO_BIN_EXE_...")`.
//!
//! Usage: `test_connector_fixture <mode> [type_name]`
//!
//! - `valid` — writes a well-formed handshake for `type_name` (default
//!   `"fixture"`) and exits.
//! - `malformed` — writes a line that isn't JSON.
//! - `bad-version` — writes a handshake with an incompatible
//!   `core_api_version`.
//! - `bad-type` — writes a handshake whose `type` fails the naming rule.
//! - `no-output` — exits immediately without writing anything.
//! - `hang` — sleeps well past any reasonable test timeout before
//!   writing anything.

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
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
