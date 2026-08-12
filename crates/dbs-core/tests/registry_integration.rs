//! Real-subprocess integration tests for `dbs_core::registry`.
//!
//! Spawns the `test_connector_fixture` binary (see
//! `src/bin/test_connector_fixture.rs`) to exercise the actual handshake
//! protocol — spawn, read a line, parse/validate — rather than only the
//! pure validation helpers already covered by `registry.rs`'s unit tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dbs_core::{ConnectorCandidate, ConnectorRegistry};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_test_connector_fixture"))
}

fn candidate(dist_name: &str, is_builtin: bool, mode: &str, type_name: &str) -> ConnectorCandidate {
    ConnectorCandidate {
        dist_name: dist_name.to_string(),
        is_builtin,
        command: fixture_path(),
        args: vec![mode.to_string(), type_name.to_string()],
    }
}

#[test]
fn discover_loads_a_valid_connector() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![candidate("rusty_dbs", true, "valid", "raindrop")];
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(5));
    assert_eq!(report.loaded.len(), 1);
    assert!(report.failures.is_empty());
    assert_eq!(report.loaded[0].type_, "raindrop");
    assert_eq!(report.loaded[0].plugin_id, "rusty_dbs:raindrop");

    assert!(registry.get("raindrop").is_some());
    assert!(registry.get("rusty_dbs:raindrop").is_some());
    assert!(registry.get("missing").is_none());
}

#[test]
fn discover_records_malformed_json_as_a_failure_without_crashing_the_others() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![
        candidate("bad", false, "malformed", "whatever"),
        candidate("rusty_dbs", true, "valid", "raindrop"),
    ];
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(5));
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(report.loaded[0].type_, "raindrop");
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0].reason.contains("malformed handshake"));
}

#[test]
fn discover_rejects_an_incompatible_core_api_version() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![candidate("rusty_dbs", true, "bad-version", "raindrop")];
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(5));
    assert!(report.loaded.is_empty());
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0].reason.contains("incompatible"));
}

#[test]
fn discover_rejects_a_malformed_connector_type() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![candidate("rusty_dbs", true, "bad-type", "unused")];
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(5));
    assert!(report.loaded.is_empty());
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0].reason.contains("must match"));
}

#[test]
fn discover_records_a_failure_when_the_connector_writes_nothing() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![candidate("rusty_dbs", true, "no-output", "unused")];
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(5));
    assert!(report.loaded.is_empty());
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0]
        .reason
        .contains("closed stdout before writing a handshake"));
}

#[test]
fn discover_times_out_a_hung_connector_instead_of_blocking_forever() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![candidate("rusty_dbs", true, "hang", "unused")];
    let started = std::time::Instant::now();
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_millis(500));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(report.loaded.is_empty());
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0].reason.contains("timed out"));
}

#[test]
fn discover_resolves_a_builtin_vs_third_party_collision() {
    let mut registry = ConnectorRegistry::new();
    let candidates = vec![
        candidate("rusty_dbs", true, "valid", "raindrop"),
        candidate("acme", false, "valid", "raindrop"),
    ];
    let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(5));
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(report.loaded[0].plugin_id, "rusty_dbs:raindrop");
    assert_eq!(report.shadowed.len(), 1);
    assert_eq!(report.shadowed[0].plugin_id, "acme:raindrop");
}
