//! Configuration loading.
//!
//! Mirrors `src/dbs/config.py` in baileyrd/Daily-Backup-System (pinned
//! `@6cc6491`). TOML only — the reference's optional YAML path
//! (`pyyaml`-gated) isn't ported; this crate has no `.yaml`/`.yml`
//! loader. Secrets are never written in the config file — they live in
//! `.env` and are referenced by `*_env` keys; the loader actively
//! *rejects* a config that inlines a secret value.
//!
//! **Scoped narrower than the reference:** `SourceConfig.export`
//! (`ExportProfileOverride`) isn't ported — `core/export_profile.py` was
//! missed as its own `gap-analysis.md` row entirely (connector.py's
//! `export_profile` class attribute has the same gap) and needs filing as
//! a follow-up issue before either can be wired in for real.
//!
//! Parsing pipeline matches the reference's order deliberately: reject
//! inline secrets *before* `${ENV}` expansion (catches both a literal
//! secret and an attempt to smuggle one into a secret-named key via
//! `${ENV}`, which would otherwise expand and then persist into a config
//! snapshot), then expand, then extract into the typed structs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::errors::DbsError;

const RESERVED_SOURCE_KEYS: &[&str] = &[
    "type",
    "enabled",
    "schedule",
    "reconcile_every_runs",
    "store_media",
    "max_media_mb",
    "requires_vpn",
    "keep_revisions",
    "export",
];

const SECRET_KEY_HINTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "api_key",
    "apikey",
    "access_key",
];

#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    pub name: String,
    pub type_: String,
    pub enabled: bool,
    pub schedule: Option<String>,
    pub reconcile_every_runs: Option<u32>,
    /// Archive media bytes into the DB for this source (opt-in).
    pub store_media: bool,
    /// Per-file size cap in MB (0 = no cap).
    pub max_media_mb: u32,
    /// Back up this source only through the configured VPN wrapper.
    pub requires_vpn: bool,
    /// Prune revision history to the newest N during `dbs maintain` (0 =
    /// keep everything).
    pub keep_revisions: u32,
    /// Connector-specific options: every key not in [`RESERVED_SOURCE_KEYS`].
    pub options: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectorOverride {
    pub plugin: Option<String>,
    pub allow_override: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VpnGuard {
    /// Refuse a `requires_vpn` source launched outside the VPN netns with
    /// an actionable message; other sources still run.
    #[default]
    Skip,
    /// Proceed but log a warning (for non-netns VPN setups).
    Warn,
    /// Disable the guard entirely.
    Off,
}

impl VpnGuard {
    fn parse(s: &str) -> Result<Self, DbsError> {
        match s {
            "skip" => Ok(Self::Skip),
            "warn" => Ok(Self::Warn),
            "off" => Ok(Self::Off),
            other => Err(DbsError::Config(format!(
                "vpn_guard must be one of skip/warn/off, not {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyOn {
    #[default]
    Failure,
    Warning,
    Always,
}

impl NotifyOn {
    fn parse(s: &str) -> Result<Self, DbsError> {
        match s {
            "failure" => Ok(Self::Failure),
            "warning" => Ok(Self::Warning),
            "always" => Ok(Self::Always),
            other => Err(DbsError::Config(format!(
                "notify_on must be one of failure/warning/always, not {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub database: String,
    pub export_dir: String,
    /// Root under which each source gets its own download folder.
    pub download_root: String,
    pub default_overlap_seconds: u32,
    pub vpn_exec: String,
    pub vpn_status: String,
    /// The Linux network namespace `vpn_exec` runs commands inside.
    pub vpn_netns: String,
    pub vpn_guard: VpnGuard,
    /// Webhook POSTed on backup completion. May be a `${ENV}` reference.
    pub notify_url: Option<String>,
    pub notify_on: NotifyOn,
    pub http_timeout: f64,
    pub http_rate_limit_per_min: u32,
    pub batch_max: u32,
    pub sweep_safety_fraction: f64,
    /// Worker pool size for `backup --all` (CLI `--parallel` overrides).
    pub parallel: u32,
    pub sources: HashMap<String, SourceConfig>,
    pub connectors: HashMap<String, ConnectorOverride>,
    pub base_dir: PathBuf,
    pub source_path: Option<PathBuf>,
}

impl Config {
    fn resolve(&self, value: &str) -> PathBuf {
        let p = PathBuf::from(shellexpand_home(value));
        if p.is_absolute() {
            p
        } else {
            self.base_dir.join(p)
        }
    }

    pub fn database_path(&self) -> PathBuf {
        self.resolve(&self.database)
    }

    pub fn export_path(&self) -> PathBuf {
        self.resolve(&self.export_dir)
    }

    pub fn download_root_path(&self) -> PathBuf {
        self.resolve(&self.download_root)
    }

    /// Per-source download folder: `<download_root>/<source-name>`.
    pub fn download_dir_for(&self, source_name: &str) -> PathBuf {
        self.download_root_path().join(source_name)
    }

    /// Translates `[connectors.<type>]` blocks into a registry override
    /// map (ADR-0001's manifest-based registry consumes this).
    pub fn registry_override(&self) -> HashMap<String, String> {
        let mut overrides = HashMap::new();
        for (ctype, ov) in &self.connectors {
            if let Some(plugin) = &ov.plugin {
                overrides.insert(ctype.clone(), plugin.clone());
            }
            if ov.allow_override {
                overrides.insert(format!("{ctype}:allow_override"), "true".to_string());
            }
        }
        overrides
    }
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn table_get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_table().and_then(|t| t.get(key))
}

fn as_str_or(value: Option<&Value>, default: &str) -> String {
    value.and_then(Value::as_str).unwrap_or(default).to_string()
}

fn as_u32_or(value: Option<&Value>, default: u32) -> u32 {
    value
        .and_then(Value::as_integer)
        .map(|v| v as u32)
        .unwrap_or(default)
}

fn as_f64_or(value: Option<&Value>, default: f64) -> f64 {
    value.and_then(Value::as_float).unwrap_or(default)
}

fn as_bool_or(value: Option<&Value>, default: bool) -> bool {
    value.and_then(Value::as_bool).unwrap_or(default)
}

/// Recursively expands `${ENV_VAR}` references in string values against
/// the process environment. A reference to an unset variable expands to
/// an empty string, matching the reference's `os.environ.get(name, "")`.
fn expand_env(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(expand_env_str(&s)),
        Value::Table(t) => Value::Table(t.into_iter().map(|(k, v)| (k, expand_env(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(expand_env).collect()),
        other => other,
    }
}

fn expand_env_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '$' && s[i..].starts_with("${") {
            if let Some(end) = s[i..].find('}') {
                let name = &s[i + 2..i + end];
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !name.chars().next().unwrap().is_ascii_digit()
                {
                    out.push_str(&std::env::var(name).unwrap_or_default());
                    for _ in 0..end {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Rejects a config that inlines what looks like a secret value: a
/// non-empty string whose key contains one of [`SECRET_KEY_HINTS`] and
/// doesn't end in `_env`.
fn reject_inline_secrets(value: &Value, path: &str) -> Result<(), DbsError> {
    match value {
        Value::Table(t) => {
            for (k, v) in t {
                let key_l = k.to_lowercase();
                let here = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if let Value::String(s) = v {
                    if !s.trim().is_empty()
                        && !key_l.ends_with("_env")
                        && SECRET_KEY_HINTS.iter().any(|h| key_l.contains(h))
                    {
                        return Err(DbsError::Config(format!(
                            "config key {here:?} looks like an inlined secret. Do not put the secret \
                             (or a ${{ENV}} reference to it) here — store the value in .env and \
                             reference it by NAME via a '*_env' key (e.g. token_env = \"RAINDROP_TOKEN\")."
                        )));
                    }
                }
                reject_inline_secrets(v, &here)?;
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                reject_inline_secrets(v, &format!("{path}[{i}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Parses a simple `.env` file (`KEY=VALUE` lines) into a map. Supports
/// `#` comments, blank lines, an optional `export` prefix, and quoted
/// values. Does not perform shell-style interpolation.
pub fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return result;
    };
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
        if !key.is_empty() {
            result.insert(key.to_string(), val);
        }
    }
    result
}

/// Loads and validates a TOML config file into a [`Config`].
pub fn load_config(path: &Path) -> Result<Config, DbsError> {
    let expanded = shellexpand_home(&path.to_string_lossy());
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err(DbsError::Config(format!(
            "config file not found: {}",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| DbsError::Config(format!("failed to read {}: {e}", path.display())))?;
    let raw: Value = toml::from_str(&text)
        .map_err(|e| DbsError::Config(format!("failed to parse {}: {e}", path.display())))?;
    if !raw.is_table() {
        return Err(DbsError::Config(
            "top-level config must be a table".to_string(),
        ));
    }

    reject_inline_secrets(&raw, "")?;
    let raw = expand_env(raw);

    let dbs_section = table_get(&raw, "dbs")
        .cloned()
        .unwrap_or(Value::Table(Default::default()));

    let mut sources = HashMap::new();
    if let Some(Value::Table(table)) = table_get(&raw, "sources") {
        for (name, body) in table {
            let Value::Table(body) = body else {
                return Err(DbsError::Config(format!("source {name:?} must be a table")));
            };
            let Some(Value::String(type_)) = body.get("type") else {
                return Err(DbsError::Config(format!(
                    "source {name:?} is missing required key 'type'"
                )));
            };
            let options: HashMap<String, Value> = body
                .iter()
                .filter(|(k, _)| !RESERVED_SOURCE_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            sources.insert(
                name.clone(),
                SourceConfig {
                    name: name.clone(),
                    type_: type_.clone(),
                    enabled: as_bool_or(body.get("enabled"), true),
                    schedule: body
                        .get("schedule")
                        .and_then(Value::as_str)
                        .map(String::from),
                    reconcile_every_runs: body
                        .get("reconcile_every_runs")
                        .and_then(Value::as_integer)
                        .map(|v| v as u32),
                    store_media: as_bool_or(body.get("store_media"), false),
                    max_media_mb: as_u32_or(body.get("max_media_mb"), 0),
                    requires_vpn: as_bool_or(body.get("requires_vpn"), false),
                    keep_revisions: as_u32_or(body.get("keep_revisions"), 0),
                    options,
                },
            );
        }
    }

    let mut connectors = HashMap::new();
    if let Some(Value::Table(table)) = table_get(&raw, "connectors") {
        for (ctype, body) in table {
            let plugin = table_get(body, "plugin")
                .and_then(Value::as_str)
                .map(String::from);
            let allow_override = as_bool_or(table_get(body, "allow_override"), false);
            connectors.insert(
                ctype.clone(),
                ConnectorOverride {
                    plugin,
                    allow_override,
                },
            );
        }
    }

    let vpn_guard = dbs_section
        .as_table()
        .and_then(|t| t.get("vpn_guard"))
        .and_then(Value::as_str)
        .map(VpnGuard::parse)
        .transpose()?
        .unwrap_or_default();
    let notify_on = dbs_section
        .as_table()
        .and_then(|t| t.get("notify_on"))
        .and_then(Value::as_str)
        .map(NotifyOn::parse)
        .transpose()?
        .unwrap_or_default();

    let resolved_path = path
        .canonicalize()
        .map_err(|e| DbsError::Config(format!("failed to resolve {}: {e}", path.display())))?;

    Ok(Config {
        database: as_str_or(table_get(&dbs_section, "database"), "dbs.sqlite3"),
        export_dir: as_str_or(table_get(&dbs_section, "export_dir"), "exports"),
        download_root: as_str_or(table_get(&dbs_section, "download_root"), "downloads"),
        default_overlap_seconds: as_u32_or(table_get(&dbs_section, "default_overlap_seconds"), 300),
        vpn_exec: as_str_or(table_get(&dbs_section, "vpn_exec"), "sudo vpn-netns exec"),
        vpn_status: as_str_or(
            table_get(&dbs_section, "vpn_status"),
            "sudo vpn-netns status",
        ),
        vpn_netns: as_str_or(table_get(&dbs_section, "vpn_netns"), "vpn"),
        vpn_guard,
        notify_url: table_get(&dbs_section, "notify_url")
            .and_then(Value::as_str)
            .map(String::from),
        notify_on,
        http_timeout: as_f64_or(table_get(&dbs_section, "http_timeout"), 30.0),
        http_rate_limit_per_min: as_u32_or(table_get(&dbs_section, "http_rate_limit_per_min"), 120),
        batch_max: as_u32_or(table_get(&dbs_section, "batch_max"), 500),
        sweep_safety_fraction: as_f64_or(table_get(&dbs_section, "sweep_safety_fraction"), 0.5),
        parallel: as_u32_or(table_get(&dbs_section, "parallel"), 1),
        sources,
        connectors,
        base_dir: resolved_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        source_path: Some(resolved_path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal scratch-file helper — avoids pulling in the `tempfile`
    /// crate for a handful of tests. Unique per call via a monotonic
    /// counter (tests run concurrently within one process).
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(contents: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rusty_dbs_config_test_{}_{n}.toml",
                std::process::id()
            ));
            std::fs::write(&path, contents).unwrap();
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            std::fs::remove_file(&self.path).ok();
        }
    }

    fn write_temp_toml(contents: &str) -> TempFile {
        TempFile::new(contents)
    }

    #[test]
    fn load_config_applies_defaults_when_dbs_section_is_absent() {
        let file = write_temp_toml("");
        let config = load_config(&file.path).unwrap();
        assert_eq!(config.database, "dbs.sqlite3");
        assert_eq!(config.parallel, 1);
        assert_eq!(config.vpn_guard, VpnGuard::Skip);
        assert_eq!(config.notify_on, NotifyOn::Failure);
    }

    #[test]
    fn load_config_missing_file_errors() {
        let err = load_config(Path::new("/nonexistent/dbs.toml")).unwrap_err();
        assert!(matches!(err, DbsError::Config(_)));
    }

    #[test]
    fn load_config_rejects_inline_secret() {
        let file = write_temp_toml(
            r#"
            [sources.raindrop]
            type = "raindrop"
            token = "abc123"
            "#,
        );
        let err = load_config(&file.path).unwrap_err();
        assert!(matches!(err, DbsError::Config(_)));
    }

    #[test]
    fn load_config_allows_token_env_key() {
        let file = write_temp_toml(
            r#"
            [sources.raindrop]
            type = "raindrop"
            token_env = "RAINDROP_TOKEN"
            "#,
        );
        let config = load_config(&file.path).unwrap();
        let source = &config.sources["raindrop"];
        assert_eq!(
            source.options.get("token_env").and_then(Value::as_str),
            Some("RAINDROP_TOKEN")
        );
    }

    #[test]
    fn load_config_source_missing_type_errors() {
        let file = write_temp_toml(
            r#"
            [sources.raindrop]
            enabled = true
            "#,
        );
        let err = load_config(&file.path).unwrap_err();
        assert!(matches!(err, DbsError::Config(_)));
    }

    #[test]
    fn load_config_expands_env_var_reference() {
        std::env::set_var("RUSTY_DBS_TEST_NOTIFY_URL", "https://example.com/hook");
        let file = write_temp_toml(
            r#"
            [dbs]
            notify_url = "${RUSTY_DBS_TEST_NOTIFY_URL}"
            "#,
        );
        let config = load_config(&file.path).unwrap();
        assert_eq!(
            config.notify_url.as_deref(),
            Some("https://example.com/hook")
        );
        std::env::remove_var("RUSTY_DBS_TEST_NOTIFY_URL");
    }

    #[test]
    fn load_config_unset_env_var_expands_to_empty_string() {
        let file = write_temp_toml(
            r#"
            [dbs]
            notify_url = "${RUSTY_DBS_DEFINITELY_UNSET_VAR}"
            "#,
        );
        let config = load_config(&file.path).unwrap();
        assert_eq!(config.notify_url.as_deref(), Some(""));
    }

    #[test]
    fn load_config_rejects_invalid_vpn_guard() {
        let file = write_temp_toml(
            r#"
            [dbs]
            vpn_guard = "bogus"
            "#,
        );
        assert!(load_config(&file.path).is_err());
    }

    #[test]
    fn registry_override_translates_connector_blocks() {
        let file = write_temp_toml(
            r#"
            [connectors.raindrop]
            plugin = "daily-backup-system:raindrop"
            allow_override = true
            "#,
        );
        let config = load_config(&file.path).unwrap();
        let overrides = config.registry_override();
        assert_eq!(
            overrides.get("raindrop"),
            Some(&"daily-backup-system:raindrop".to_string())
        );
        assert_eq!(
            overrides.get("raindrop:allow_override"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn download_dir_for_joins_download_root_and_source_name() {
        let file = write_temp_toml("");
        let config = load_config(&file.path).unwrap();
        let dir = config.download_dir_for("raindrop");
        assert_eq!(dir.file_name().unwrap(), "raindrop");
        assert!(dir.starts_with(config.download_root_path()));
    }

    #[test]
    fn parse_env_file_handles_comments_export_and_quotes() {
        let path =
            std::env::temp_dir().join(format!("rusty_dbs_env_test_{}.env", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# a comment").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "export TOKEN=\"abc123\"").unwrap();
        writeln!(f, "PLAIN=value").unwrap();
        writeln!(f, "SINGLE='quoted'").unwrap();
        drop(f);

        let parsed = parse_env_file(&path);
        assert_eq!(parsed.get("TOKEN"), Some(&"abc123".to_string()));
        assert_eq!(parsed.get("PLAIN"), Some(&"value".to_string()));
        assert_eq!(parsed.get("SINGLE"), Some(&"quoted".to_string()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_env_file_missing_file_returns_empty_map() {
        assert!(parse_env_file(Path::new("/nonexistent/.env")).is_empty());
    }

    #[test]
    fn expand_env_str_leaves_malformed_references_untouched() {
        assert_eq!(expand_env_str("no reference here"), "no reference here");
        assert_eq!(expand_env_str("${}"), "${}");
        assert_eq!(expand_env_str("$NOT_BRACED"), "$NOT_BRACED");
    }
}
