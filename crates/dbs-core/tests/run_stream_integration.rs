//! Real-subprocess integration tests for `dbs_core::run_stream` (issue
//! #157) — spawns the `test_connector_fixture` binary's `run` scenarios
//! (see `src/bin/test_connector_fixture.rs`) to exercise the actual
//! run/stream protocol end to end: real process spawn, a real JSON line
//! written to its stdin, real JSON lines read back from its stdout, and
//! real commits against an in-memory `SqliteStorage`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dbs_core::{
    run_connector_subprocess, CancelToken, Capabilities, Cursor, Handshake, RegisteredConnector,
    SqliteStorage, Storage, WireRunContext, CURRENT_API_VERSION,
};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_test_connector_fixture"))
}

fn connector(args: &[&str], capabilities: Capabilities) -> RegisteredConnector {
    RegisteredConnector {
        type_: "fixture".to_string(),
        plugin_id: "rusty_dbs:fixture".to_string(),
        dist_name: "rusty_dbs".to_string(),
        is_builtin: true,
        handshake: Handshake {
            type_: "fixture".to_string(),
            core_api_version: CURRENT_API_VERSION,
            schema_version: 1,
            capabilities,
            secret_keys: Vec::new(),
            item_kinds: vec!["item".to_string()],
            display_name: None,
            description: None,
            export_profile: None,
            auth_capture: None,
            volatile_fields: Vec::new(),
            pip_requirements: Vec::new(),
            needs_playwright_browser: false,
        },
        command: fixture_path(),
        args: args.iter().map(|s| s.to_string()).collect(),
    }
}

fn open_storage_with_source() -> (SqliteStorage, i64) {
    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("fixture-source", "fixture", "rusty_dbs:fixture", "{}", 1)
        .unwrap();
    (storage, source.id)
}

fn wire_ctx(source_id: i64, run_id: i64, mode: &str) -> WireRunContext {
    WireRunContext {
        source_id,
        source_name: "fixture-source".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!({"page": 1}),
        }),
        since: None,
        secrets: HashMap::new(),
        run_id,
        mode: mode.to_string(),
        full_refresh: mode == "full",
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
    }
}

#[test]
fn a_clean_run_commits_items_and_reports_success() {
    let (mut storage, source_id) = open_storage_with_source();
    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "incremental", None)
        .unwrap();
    let rc = connector(&["run", "ok", "3"], Capabilities::default());
    let ctx = wire_ctx(source_id, run_id, "incremental");

    let outcome = run_connector_subprocess(&mut storage, &rc, ctx, 0.5, None).unwrap();

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 3);
    assert_eq!(outcome.stats.created, 3);
    assert_eq!(
        outcome.cursor_after.unwrap().value,
        serde_json::json!({"page": 2})
    );
}

#[test]
fn the_wire_context_the_connector_receives_matches_what_the_host_sent() {
    let (mut storage, source_id) = open_storage_with_source();
    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "full", None)
        .unwrap();
    let rc = connector(&["run", "ok", "1"], Capabilities::default());
    let mut ctx = wire_ctx(source_id, run_id, "full");
    ctx.limit = Some(50);
    ctx.secrets.insert("TOKEN".to_string(), "shh".to_string());

    run_connector_subprocess(&mut storage, &rc, ctx, 0.5, None).unwrap();

    let query = dbs_core::ExportQuery {
        sources: Some(vec!["fixture-source".to_string()]),
        ..Default::default()
    };
    let (rows, _total) = storage.browse_items(&query, None, 10, 0).unwrap();
    let item_id = rows[0]["id"].as_i64().unwrap();
    let detail = storage.get_item(item_id).unwrap().unwrap();
    let echoed = &detail["raw"]["_wire_ctx"];
    assert_eq!(echoed["mode"], "full");
    assert_eq!(echoed["full_refresh"], true);
    assert_eq!(echoed["limit"], 50);
    assert_eq!(echoed["secrets"]["TOKEN"], "shh");
    assert_eq!(echoed["source_name"], "fixture-source");
}

#[test]
fn a_full_enumeration_run_sweeps_items_missing_from_the_reconcile_marker() {
    let (mut storage, source_id) = open_storage_with_source();
    // Seed a run: an item "keep" and an item "drop" both exist, then a
    // reconcile run reports only "keep" as live — "drop" should be
    // soft-deleted.
    let seed_run = storage
        .begin_run(source_id, "rusty_dbs:fixture", "full", None)
        .unwrap();
    let rc = connector(
        &["run", "ok", "0"],
        Capabilities {
            supports_full_enumeration: true,
            ..Capabilities::default()
        },
    );
    let _ = run_connector_subprocess(
        &mut storage,
        &rc,
        wire_ctx(source_id, seed_run, "full"),
        0.5,
        None,
    );

    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "full", None)
        .unwrap();
    let rc = connector(
        &["run", "reconcile"],
        Capabilities {
            supports_full_enumeration: true,
            ..Capabilities::default()
        },
    );
    let outcome = run_connector_subprocess(
        &mut storage,
        &rc,
        wire_ctx(source_id, run_id, "full"),
        0.5,
        None,
    )
    .unwrap();

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let live = storage.live_external_ids(source_id, None).unwrap();
    assert!(live.contains("keep"));
    assert!(!live.contains("drop"));
}

#[test]
fn a_connector_reported_error_is_partial_when_items_were_already_committed() {
    let (mut storage, source_id) = open_storage_with_source();
    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "incremental", None)
        .unwrap();
    // First establish a committed checkpoint, then a second run that
    // reports an error after one item but before any checkpoint —
    // that item should NOT survive (mirrors the reference: an
    // exception mid-stream skips the trailing flush entirely).
    let rc = connector(
        &["run", "error", "transient", "upstream exploded"],
        Capabilities::default(),
    );
    let outcome = run_connector_subprocess(
        &mut storage,
        &rc,
        wire_ctx(source_id, run_id, "incremental"),
        0.5,
        None,
    )
    .unwrap();

    assert!(outcome
        .error
        .as_deref()
        .unwrap()
        .contains("upstream exploded"));
    // Nothing was ever committed before the error, so Failed not Partial.
    assert!(matches!(outcome.status, dbs_core::RunStatus::Failed));
    assert_eq!(outcome.items_seen, 1);
    assert_eq!(
        outcome.stats.created, 0,
        "the buffered item must not commit"
    );
}

#[test]
fn a_malformed_line_is_a_contract_violation() {
    let (mut storage, source_id) = open_storage_with_source();
    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "incremental", None)
        .unwrap();
    let rc = connector(&["run", "malformed"], Capabilities::default());
    let outcome = run_connector_subprocess(
        &mut storage,
        &rc,
        wire_ctx(source_id, run_id, "incremental"),
        0.5,
        None,
    )
    .unwrap();

    let err = outcome.error.unwrap();
    assert!(err.contains("malformed line"), "{err}");
}

#[test]
fn exiting_without_a_terminal_line_is_a_contract_violation() {
    let (mut storage, source_id) = open_storage_with_source();
    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "incremental", None)
        .unwrap();
    let rc = connector(&["run", "no-terminal"], Capabilities::default());
    let outcome = run_connector_subprocess(
        &mut storage,
        &rc,
        wire_ctx(source_id, run_id, "incremental"),
        0.5,
        None,
    )
    .unwrap();

    let err = outcome.error.unwrap();
    assert!(err.contains("protocol violation"), "{err}");
}

#[test]
fn cancelling_mid_run_actually_kills_the_child_instead_of_waiting_for_it() {
    let (mut storage, source_id) = open_storage_with_source();
    let run_id = storage
        .begin_run(source_id, "rusty_dbs:fixture", "incremental", None)
        .unwrap();
    let rc = connector(&["run", "hang"], Capabilities::default());
    let cancel = CancelToken::new();
    let cancel_for_thread = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        cancel_for_thread.cancel();
    });

    let started = Instant::now();
    let outcome = run_connector_subprocess(
        &mut storage,
        &rc,
        wire_ctx(source_id, run_id, "incremental"),
        0.5,
        Some(&cancel),
    )
    .unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "run_connector_subprocess should return promptly once cancelled and the \
         child killed, not wait out the fixture's 1-hour sleep (took {elapsed:?})"
    );
    assert!(matches!(outcome.status, dbs_core::RunStatus::Interrupted));
}
