//! Connector discovery and the plugin registry.
//!
//! Mirrors `src/dbs/core/registry.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`), adapted to ADR-0001's subprocess + line-delimited
//! JSON-IPC design (`docs/adr/0001-dynamic-plugin-registry.md`, issue
//! #5): Python discovers connectors via `importlib.metadata` entry
//! points and validates a loaded *class*'s attributes; this port
//! discovers them by spawning each candidate subprocess and validating a
//! **handshake JSON line** the subprocess itself writes on startup. The
//! validation rules (type format, capabilities coherence, item_kinds
//! non-empty, secret_keys required when `requires_auth`, core API
//! version compatibility) and the collision-resolution precedence
//! (explicit override > built-in shadow protection > deterministic
//! third-party sort) port directly — both operate on manifest/handshake
//! data, not on Rust/Python types, so nothing about moving to IPC
//! changes that logic.
//!
//! **Scope note:** this issue implements the handshake protocol,
//! contract validation, version gating, and collision resolution given
//! an already-resolved list of candidate connector commands
//! ([`ConnectorCandidate`]). Enumerating candidates from a directory
//! scan of `dbs-connector-*` binaries or a `connectors.toml` manifest —
//! the ADR's "replaces entry-point metadata" step — is deferred to the
//! CLI issue that needs it (`dbs sources`/`dbs connectors`), which
//! already has to resolve a connectors directory/PATH from config.
//! Likewise, this issue only implements the handshake half of the
//! protocol (steps 1 and 4 in the ADR); the run/stream half (steps 2-3 —
//! writing a `RunContext` and reading `FetchEvent` lines back) is a
//! separate concern for whichever issue bridges a [`RegisteredConnector`]
//! to the `Connector` trait's `fetch` signature.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;

use crate::capabilities::Capabilities;
use crate::versioning::{is_api_compatible, CURRENT_API_VERSION};

/// Default time budget for a connector to write its handshake line after
/// spawn — generous (subprocess start + import time), but finite so a
/// hung connector can never block discovery of the others. Callers may
/// pass a shorter value (e.g. in tests) via [`ConnectorRegistry::discover`].
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// One connector subprocess discovery should try to spawn and handshake
/// with. The caller resolves this list (directory scan, manifest file,
/// config) — see the module doc-comment's scope note.
#[derive(Debug, Clone)]
pub struct ConnectorCandidate {
    /// Distribution/package name this candidate came from — used for
    /// `plugin_id` (`"<dist>:<type>"`) and collision precedence. The
    /// reference derives `is_builtin` by comparing this against its own
    /// package name; here the caller states it directly since there's no
    /// package-metadata equivalent to introspect.
    pub dist_name: String,
    pub is_builtin: bool,
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// The JSON line a connector subprocess writes on startup, self-describing
/// its contract. Field names match the reference's `_validate_contract`
/// checks (`src/dbs/core/registry.py`) and ADR-0001's handshake shape.
#[derive(Debug, Clone, Deserialize)]
pub struct Handshake {
    #[serde(rename = "type")]
    pub type_: String,
    pub core_api_version: u32,
    pub schema_version: u32,
    pub capabilities: Capabilities,
    #[serde(default)]
    pub secret_keys: Vec<String>,
    pub item_kinds: Vec<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// A successfully loaded, contract-valid connector.
#[derive(Debug, Clone)]
pub struct RegisteredConnector {
    pub type_: String,
    /// `"<dist_name>:<type>"`.
    pub plugin_id: String,
    pub dist_name: String,
    pub is_builtin: bool,
    pub handshake: Handshake,
    /// Kept so a future run/stream issue can re-spawn this exact
    /// connector without re-discovering it.
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// A connector candidate that could not be loaded or validated. Mirrors
/// the reference's `LoadFailure` (its `entry_point` field has no
/// subprocess analogue, so this just keeps `dist_name` + `reason`).
#[derive(Debug, Clone)]
pub struct LoadFailure {
    pub dist_name: String,
    pub reason: String,
}

/// Result of [`ConnectorRegistry::discover`].
#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    pub loaded: Vec<RegisteredConnector>,
    pub failures: Vec<LoadFailure>,
    pub shadowed: Vec<RegisteredConnector>,
}

/// Loads, validates, and resolves connectors with collision precedence.
#[derive(Debug, Default)]
pub struct ConnectorRegistry {
    by_type: HashMap<String, RegisteredConnector>,
    by_plugin_id: HashMap<String, RegisteredConnector>,
    report: LoadReport,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry directly from already-resolved connectors,
    /// bypassing spawn/handshake/collision-resolution — for tests (e.g.
    /// `BackupService`'s, #46) and any caller that already has
    /// [`RegisteredConnector`] values from another source. If two
    /// entries share a `type`, the later one in `connectors` wins the
    /// `by_type` lookup; every entry is always reachable by its own
    /// `plugin_id` regardless.
    pub fn from_resolved(connectors: impl IntoIterator<Item = RegisteredConnector>) -> Self {
        let mut registry = Self::new();
        for rc in connectors {
            registry
                .by_plugin_id
                .insert(rc.plugin_id.clone(), rc.clone());
            registry.by_type.insert(rc.type_.clone(), rc);
        }
        registry
    }

    /// Spawns and handshakes with every candidate (each given up to
    /// `timeout` to write its handshake line), validates contracts, then
    /// resolves collisions. `override_map` maps `type -> plugin_id` to
    /// force a specific provider, plus the special key
    /// `"<type>:allow_override"` = `"true"` to let a third party shadow a
    /// built-in — same semantics as the reference's `override` parameter.
    ///
    /// A candidate that fails to spawn, hangs past `timeout`, writes
    /// malformed JSON, or fails contract validation never crashes
    /// discovery of the others — it's recorded in the returned report's
    /// `failures` instead.
    pub fn discover(
        &mut self,
        candidates: &[ConnectorCandidate],
        override_map: &HashMap<String, String>,
        timeout: Duration,
    ) -> &LoadReport {
        self.by_type.clear();
        self.by_plugin_id.clear();
        self.report = LoadReport::default();

        let mut grouped: HashMap<String, Vec<RegisteredConnector>> = HashMap::new();
        for candidate in candidates {
            match handshake_and_validate(candidate, timeout) {
                Ok(handshake) => {
                    let plugin_id = format!("{}:{}", candidate.dist_name, handshake.type_);
                    let rc = RegisteredConnector {
                        type_: handshake.type_.clone(),
                        plugin_id: plugin_id.clone(),
                        dist_name: candidate.dist_name.clone(),
                        is_builtin: candidate.is_builtin,
                        handshake,
                        command: candidate.command.clone(),
                        args: candidate.args.clone(),
                    };
                    self.by_plugin_id.insert(plugin_id, rc.clone());
                    grouped.entry(rc.type_.clone()).or_default().push(rc);
                }
                Err(reason) => {
                    self.report.failures.push(LoadFailure {
                        dist_name: candidate.dist_name.clone(),
                        reason,
                    });
                }
            }
        }

        let mut types: Vec<String> = grouped.keys().cloned().collect();
        types.sort();
        for ctype in types {
            let group = grouped.remove(&ctype).expect("just collected this key");
            match pick_winner(&ctype, &group, override_map) {
                Ok(winner) => {
                    for other in &group {
                        if other.plugin_id != winner.plugin_id {
                            self.report.shadowed.push(other.clone());
                        }
                    }
                    self.by_type.insert(ctype, winner.clone());
                    self.report.loaded.push(winner);
                }
                Err(reason) => {
                    self.report.failures.push(LoadFailure {
                        dist_name: "(override)".to_string(),
                        reason,
                    });
                }
            }
        }

        &self.report
    }

    /// Looks up by plugin id first, then by bare type — matches the
    /// reference's `get`.
    pub fn get(&self, type_or_plugin_id: &str) -> Option<&RegisteredConnector> {
        self.by_plugin_id
            .get(type_or_plugin_id)
            .or_else(|| self.by_type.get(type_or_plugin_id))
    }

    /// Every resolved connector, sorted by type.
    pub fn all(&self) -> Vec<&RegisteredConnector> {
        let mut v: Vec<&RegisteredConnector> = self.by_type.values().collect();
        v.sort_by(|a, b| a.type_.cmp(&b.type_));
        v
    }

    pub fn report(&self) -> &LoadReport {
        &self.report
    }
}

fn pick_winner(
    ctype: &str,
    group: &[RegisteredConnector],
    override_map: &HashMap<String, String>,
) -> Result<RegisteredConnector, String> {
    // 1. Explicit config override wins outright. A forced plugin_id that
    //    matches nothing is a misconfiguration and must fail loudly
    //    rather than silently selecting a different provider.
    if let Some(forced) = override_map.get(ctype) {
        return group
            .iter()
            .find(|rc| &rc.plugin_id == forced)
            .cloned()
            .ok_or_else(|| {
                let mut available: Vec<&str> =
                    group.iter().map(|rc| rc.plugin_id.as_str()).collect();
                available.sort();
                format!(
                    "config forces connector type {ctype:?} to plugin {forced:?}, but no \
                     installed plugin has that id. Available: {available:?}"
                )
            });
    }
    if group.len() == 1 {
        return Ok(group[0].clone());
    }

    let builtin = group.iter().find(|rc| rc.is_builtin);
    let third_parties: Vec<&RegisteredConnector> =
        group.iter().filter(|rc| !rc.is_builtin).collect();
    let allow_override = override_map
        .get(&format!("{ctype}:allow_override"))
        .is_some_and(|v| v == "true");

    // 2. Built-in shadow protection: a third party overrides a built-in
    //    only with explicit allow_override.
    if let Some(b) = builtin {
        if !allow_override {
            return Ok(b.clone());
        }
    }

    // 3. Deterministic resolution among third parties (stable sort).
    let pool: Vec<&RegisteredConnector> = if third_parties.is_empty() {
        group.iter().collect()
    } else {
        third_parties
    };
    let mut sorted = pool;
    sorted.sort_by(|a, b| {
        (a.dist_name.as_str(), a.plugin_id.as_str())
            .cmp(&(b.dist_name.as_str(), b.plugin_id.as_str()))
    });
    Ok(sorted[0].clone())
}

fn handshake_and_validate(
    candidate: &ConnectorCandidate,
    timeout: Duration,
) -> Result<Handshake, String> {
    let line = spawn_and_read_handshake_line(candidate, timeout)?;
    let handshake: Handshake =
        serde_json::from_str(&line).map_err(|e| format!("malformed handshake JSON: {e}"))?;
    validate_contract(&handshake)?;
    Ok(handshake)
}

fn validate_contract(h: &Handshake) -> Result<(), String> {
    if !is_valid_connector_type(&h.type_) {
        return Err(format!(
            "connector.type {:?} must match ^[a-z][a-z0-9_]*$",
            h.type_
        ));
    }
    h.capabilities
        .assert_coherent()
        .map_err(|e| format!("connector.capabilities: {e}"))?;
    if h.item_kinds.is_empty() {
        return Err("connector must declare at least one item kind".to_string());
    }
    if h.capabilities.requires_auth && h.secret_keys.is_empty() {
        return Err("requires_auth=true but no secret_keys declared".to_string());
    }
    if !is_api_compatible(h.core_api_version) {
        return Err(format!(
            "core_api_version {} is incompatible; rebuild the connector against core API v{CURRENT_API_VERSION}",
            h.core_api_version
        ));
    }
    Ok(())
}

fn is_valid_connector_type(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Spawns `candidate`, waits up to `timeout` for one line on its stdout,
/// then kills it — discovery only needs the handshake, not a live
/// process (see the module doc-comment's scope note on the run/stream
/// half being separate). The read happens on a worker thread since a
/// blocking `read_line` has no built-in deadline; `recv_timeout` on the
/// result channel is what actually enforces `timeout`.
fn spawn_and_read_handshake_line(
    candidate: &ConnectorCandidate,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new(&candidate.command)
        .args(&candidate.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn: {e}"))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = match reader.read_line(&mut line) {
            Ok(0) => Err("connector closed stdout before writing a handshake".to_string()),
            Ok(_) => Ok(line.trim_end().to_string()),
            Err(e) => Err(format!("failed to read handshake: {e}")),
        };
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = child.kill();
            let _ = child.wait();
            result
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            // The reader thread is left to notice the closed pipe and
            // exit on its own — not worth blocking discovery on it.
            Err(format!(
                "handshake timed out after {:.1}s",
                timeout.as_secs_f64()
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.wait();
            Err("handshake reader thread ended without a result".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_connector_type_matches_the_reference_pattern() {
        assert!(is_valid_connector_type("raindrop"));
        assert!(is_valid_connector_type("a1_b2"));
        assert!(!is_valid_connector_type(""));
        assert!(!is_valid_connector_type("Raindrop"));
        assert!(!is_valid_connector_type("1abc"));
        assert!(!is_valid_connector_type("bad-type"));
    }

    #[test]
    fn validate_contract_rejects_missing_item_kinds() {
        let h = Handshake {
            type_: "fixture".to_string(),
            core_api_version: CURRENT_API_VERSION,
            schema_version: 1,
            capabilities: Capabilities::default(),
            secret_keys: Vec::new(),
            item_kinds: Vec::new(),
            display_name: None,
            description: None,
        };
        assert!(validate_contract(&h).is_err());
    }

    #[test]
    fn validate_contract_requires_secret_keys_when_auth_is_required() {
        let h = Handshake {
            type_: "fixture".to_string(),
            core_api_version: CURRENT_API_VERSION,
            schema_version: 1,
            capabilities: Capabilities {
                requires_auth: true,
                ..Capabilities::default()
            },
            secret_keys: Vec::new(),
            item_kinds: vec!["item".to_string()],
            display_name: None,
            description: None,
        };
        assert!(validate_contract(&h).is_err());
    }

    #[test]
    fn validate_contract_rejects_incompatible_core_api_version() {
        let h = Handshake {
            type_: "fixture".to_string(),
            core_api_version: CURRENT_API_VERSION + 1,
            schema_version: 1,
            capabilities: Capabilities {
                requires_auth: false,
                ..Capabilities::default()
            },
            secret_keys: Vec::new(),
            item_kinds: vec!["item".to_string()],
            display_name: None,
            description: None,
        };
        let err = validate_contract(&h).unwrap_err();
        assert!(err.contains("incompatible"));
    }

    #[test]
    fn validate_contract_accepts_a_well_formed_handshake() {
        // Capabilities::default() has requires_auth = true (matching the
        // reference's own class-level default) — set it false here so
        // this handshake, which declares no secret_keys, is coherent.
        let h = Handshake {
            type_: "fixture".to_string(),
            core_api_version: CURRENT_API_VERSION,
            schema_version: 1,
            capabilities: Capabilities {
                requires_auth: false,
                ..Capabilities::default()
            },
            secret_keys: Vec::new(),
            item_kinds: vec!["item".to_string()],
            display_name: None,
            description: None,
        };
        assert!(validate_contract(&h).is_ok());
    }

    fn rc(dist_name: &str, plugin_id: &str, is_builtin: bool) -> RegisteredConnector {
        RegisteredConnector {
            type_: "raindrop".to_string(),
            plugin_id: plugin_id.to_string(),
            dist_name: dist_name.to_string(),
            is_builtin,
            handshake: Handshake {
                type_: "raindrop".to_string(),
                core_api_version: CURRENT_API_VERSION,
                schema_version: 1,
                capabilities: Capabilities::default(),
                secret_keys: Vec::new(),
                item_kinds: vec!["link".to_string()],
                display_name: None,
                description: None,
            },
            command: PathBuf::from("dbs-connector-raindrop"),
            args: Vec::new(),
        }
    }

    #[test]
    fn pick_winner_single_candidate_wins_by_default() {
        let group = vec![rc("rusty_dbs", "rusty_dbs:raindrop", true)];
        let winner = pick_winner("raindrop", &group, &HashMap::new()).unwrap();
        assert_eq!(winner.plugin_id, "rusty_dbs:raindrop");
    }

    #[test]
    fn pick_winner_builtin_shadows_third_party_by_default() {
        let group = vec![
            rc("rusty_dbs", "rusty_dbs:raindrop", true),
            rc("acme", "acme:raindrop", false),
        ];
        let winner = pick_winner("raindrop", &group, &HashMap::new()).unwrap();
        assert_eq!(winner.plugin_id, "rusty_dbs:raindrop");
    }

    #[test]
    fn pick_winner_allow_override_lets_third_party_win() {
        let group = vec![
            rc("rusty_dbs", "rusty_dbs:raindrop", true),
            rc("acme", "acme:raindrop", false),
        ];
        let mut overrides = HashMap::new();
        overrides.insert("raindrop:allow_override".to_string(), "true".to_string());
        let winner = pick_winner("raindrop", &group, &overrides).unwrap();
        assert_eq!(winner.plugin_id, "acme:raindrop");
    }

    #[test]
    fn pick_winner_third_parties_resolve_deterministically() {
        let group = vec![
            rc("zeta", "zeta:raindrop", false),
            rc("acme", "acme:raindrop", false),
        ];
        let winner = pick_winner("raindrop", &group, &HashMap::new()).unwrap();
        assert_eq!(winner.plugin_id, "acme:raindrop");
        // Order in `group` shouldn't matter — same winner either way.
        let group_reordered = vec![
            rc("acme", "acme:raindrop", false),
            rc("zeta", "zeta:raindrop", false),
        ];
        let winner2 = pick_winner("raindrop", &group_reordered, &HashMap::new()).unwrap();
        assert_eq!(winner2.plugin_id, "acme:raindrop");
    }

    #[test]
    fn pick_winner_explicit_override_wins_outright() {
        let group = vec![
            rc("rusty_dbs", "rusty_dbs:raindrop", true),
            rc("acme", "acme:raindrop", false),
        ];
        let mut overrides = HashMap::new();
        overrides.insert("raindrop".to_string(), "acme:raindrop".to_string());
        let winner = pick_winner("raindrop", &group, &overrides).unwrap();
        assert_eq!(winner.plugin_id, "acme:raindrop");
    }

    #[test]
    fn pick_winner_forced_override_to_an_unknown_plugin_id_errors() {
        let group = vec![rc("rusty_dbs", "rusty_dbs:raindrop", true)];
        let mut overrides = HashMap::new();
        overrides.insert("raindrop".to_string(), "nonexistent:raindrop".to_string());
        assert!(pick_winner("raindrop", &group, &overrides).is_err());
    }

    #[test]
    fn discover_with_no_candidates_reports_nothing() {
        let mut registry = ConnectorRegistry::new();
        let report = registry.discover(&[], &HashMap::new(), Duration::from_secs(1));
        assert!(report.loaded.is_empty());
        assert!(report.failures.is_empty());
        assert!(report.shadowed.is_empty());
        assert!(registry.all().is_empty());
    }

    #[test]
    fn discover_reports_a_spawn_failure_for_a_nonexistent_command() {
        let mut registry = ConnectorRegistry::new();
        let candidates = vec![ConnectorCandidate {
            dist_name: "rusty_dbs".to_string(),
            is_builtin: true,
            command: PathBuf::from("this-binary-does-not-exist-anywhere"),
            args: Vec::new(),
        }];
        let report = registry.discover(&candidates, &HashMap::new(), Duration::from_secs(1));
        assert!(report.loaded.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].reason.contains("failed to spawn"));
    }

    #[test]
    fn from_resolved_is_reachable_by_both_type_and_plugin_id() {
        let entry = rc("rusty_dbs", "rusty_dbs:raindrop", true);
        let registry = ConnectorRegistry::from_resolved([entry]);
        assert!(registry.get("raindrop").is_some());
        assert!(registry.get("rusty_dbs:raindrop").is_some());
        assert_eq!(registry.all().len(), 1);
    }
}
