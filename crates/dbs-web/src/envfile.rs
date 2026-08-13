//! Minimal, dependency-free `.env` writer for the (future) web
//! Secrets UI (issue #82's own scope — the setup routes that call it
//! are #83). Reads/writes the same `KEY=VALUE` format
//! [`dbs_core::parse_env_file`] understands: this module is the write
//! side of that read path, kept as narrow as the reference's
//! `envfile.py` — upsert/remove a single key while preserving the rest
//! of the file (comments, ordering, unrelated keys), and refuse values
//! that could inject extra lines.
//!
//! Secrets belong in `.env` (gitignored), never in the TOML config;
//! this is the write path that keeps that invariant true when a secret
//! is set from the (future) UI. It never logs or returns a secret
//! value — callers get a key set or a bool, not content.
//!
//! This is the one place `dbs-web` depends on `dbs-core`, purely to
//! reuse [`dbs_core::parse_env_file`] for [`read_keys`] rather than
//! duplicating its parsing rules (comments, `export`, quote-stripping)
//! a second time.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::Path;

use dbs_core::parse_env_file;

#[derive(Debug)]
pub enum EnvFileError {
    /// `key` isn't a valid env var name, or `value` can't be safely
    /// written (contains a newline, carriage return, or double-quote).
    Invalid(String),
    Io(io::Error),
}

impl fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "{msg}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EnvFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(_) => None,
            Self::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for EnvFileError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Rejects a `key`/`value` pair that can't be safely written: `key`
/// must be a valid env var name (`^[A-Za-z_][A-Za-z0-9_]*$`), and
/// `value` may not contain a newline, carriage return, or double-quote
/// (every written value is double-quoted, so an embedded quote or
/// newline would let it inject extra lines/keys into the file).
pub fn validate(key: &str, value: &str) -> Result<(), EnvFileError> {
    if !valid_key(key) {
        return Err(EnvFileError::Invalid(format!(
            "invalid env var name: {key:?}"
        )));
    }
    if value.contains(['\n', '\r', '"']) {
        return Err(EnvFileError::Invalid(
            "value may not contain newlines or double-quotes".to_string(),
        ));
    }
    Ok(())
}

/// The env var a `.env` line assigns, or `None` for a blank line, a
/// `#` comment, or a line with no `=`.
fn line_key(line: &str) -> Option<&str> {
    let s = line.trim();
    if s.is_empty() || s.starts_with('#') || !s.contains('=') {
        return None;
    }
    let s = s.strip_prefix("export ").unwrap_or(s);
    let key = s.split_once('=')?.0.trim();
    (!key.is_empty()).then_some(key)
}

fn format_assignment(key: &str, value: &str) -> String {
    // parse_env_file strips one layer of surrounding quotes, so
    // quoting lets whitespace-bearing values round-trip; `validate`
    // already rejected embedded quotes.
    format!("{key}=\"{value}\"")
}

/// Creates or updates `key` in the `.env` at `path`, preserving every
/// other line (comments, ordering, unrelated keys) — creating the file
/// (and its parent directories) if it doesn't exist yet. A duplicate
/// existing assignment of `key` collapses to the single new one, at
/// the first occurrence's position.
///
/// A newly-created file is `chmod`'d `0600` (owner read/write only) —
/// best-effort, matching the reference: a secrets file shouldn't be
/// world-readable, but a `chmod` failure isn't fatal to having written
/// the secret. Unix-only: Windows has no equivalent POSIX-mode
/// permission bit for this crate to set, same gap the reference itself
/// has there (`Path.chmod` is a same no-op-ish call on Windows).
pub fn set_var(path: &Path, key: &str, value: &str) -> Result<(), EnvFileError> {
    validate(key, value)?;
    let existed = path.exists();
    let lines: Vec<String> = if existed {
        std::fs::read_to_string(path)?
            .lines()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };

    let mut out = Vec::with_capacity(lines.len() + 1);
    let mut replaced = false;
    for line in &lines {
        if line_key(line) == Some(key) {
            if !replaced {
                out.push(format_assignment(key, value));
                replaced = true;
            }
            // drop any further duplicate assignments of the same key
        } else {
            out.push(line.clone());
        }
    }
    if !replaced {
        out.push(format_assignment(key, value));
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, out.join("\n") + "\n")?;

    if !existed {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
}

/// Removes every assignment of `key` from the `.env` at `path`.
/// Returns `true` iff something was removed (`false` for a missing
/// file, or a file that never assigned `key`).
pub fn unset_var(path: &Path, key: &str) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line_key(line) != Some(key))
        .collect();
    if kept.len() == lines.len() {
        return Ok(false);
    }
    let content = if kept.is_empty() {
        String::new()
    } else {
        kept.join("\n") + "\n"
    };
    std::fs::write(path, content)?;
    Ok(true)
}

/// The set of keys currently assigned a non-empty value in the `.env`
/// at `path` (empty file/missing file yields an empty set). Never
/// returns the values themselves — a caller checking "is this secret
/// set" doesn't need to see it.
pub fn read_keys(path: &Path) -> HashSet<String> {
    parse_env_file(path)
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, _)| k)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dbs-web-envfile-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn set_var_creates_a_new_file_with_the_key() {
        let path = temp_path("create");
        set_var(&path, "RAINDROP_TOKEN", "abc123").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "RAINDROP_TOKEN=\"abc123\"\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_var_updates_an_existing_key_in_place_and_preserves_others() {
        let path = temp_path("update");
        std::fs::write(
            &path,
            "# a comment\nFOO=\"bar\"\nRAINDROP_TOKEN=\"old\"\nBAZ=\"qux\"\n",
        )
        .unwrap();
        set_var(&path, "RAINDROP_TOKEN", "new").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents,
            "# a comment\nFOO=\"bar\"\nRAINDROP_TOKEN=\"new\"\nBAZ=\"qux\"\n"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_var_collapses_duplicate_existing_assignments_to_one() {
        let path = temp_path("dedup");
        std::fs::write(&path, "KEY=\"first\"\nKEY=\"second\"\n").unwrap();
        set_var(&path, "KEY", "third").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "KEY=\"third\"\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_var_recognizes_the_export_prefix_when_matching_the_key() {
        let path = temp_path("export-prefix");
        std::fs::write(&path, "export KEY=\"old\"\n").unwrap();
        set_var(&path, "KEY", "new").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "KEY=\"new\"\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_var_rejects_an_invalid_key_name() {
        let path = temp_path("bad-key");
        let err = set_var(&path, "1NOT-VALID", "x").unwrap_err();
        assert!(matches!(err, EnvFileError::Invalid(_)));
        assert!(!path.exists());
    }

    #[test]
    fn set_var_rejects_a_value_with_an_embedded_quote() {
        let path = temp_path("bad-value-quote");
        let err = set_var(&path, "KEY", "has\"quote").unwrap_err();
        assert!(matches!(err, EnvFileError::Invalid(_)));
        assert!(!path.exists());
    }

    #[test]
    fn set_var_rejects_a_value_with_a_newline() {
        let path = temp_path("bad-value-newline");
        let err = set_var(&path, "KEY", "a\nFAKE_KEY=injected").unwrap_err();
        assert!(matches!(err, EnvFileError::Invalid(_)));
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn set_var_chmods_a_newly_created_file_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("chmod");
        set_var(&path, "KEY", "value").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unset_var_removes_every_assignment_and_preserves_the_rest() {
        let path = temp_path("unset");
        std::fs::write(&path, "FOO=\"bar\"\nKEY=\"a\"\nKEY=\"b\"\nBAZ=\"qux\"\n").unwrap();
        let removed = unset_var(&path, "KEY").unwrap();
        assert!(removed);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "FOO=\"bar\"\nBAZ=\"qux\"\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unset_var_on_a_missing_file_returns_false() {
        let path = temp_path("unset-missing");
        assert!(!unset_var(&path, "KEY").unwrap());
    }

    #[test]
    fn unset_var_returns_false_when_the_key_was_never_present() {
        let path = temp_path("unset-absent-key");
        std::fs::write(&path, "FOO=\"bar\"\n").unwrap();
        assert!(!unset_var(&path, "KEY").unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_keys_returns_only_non_empty_valued_keys() {
        let path = temp_path("read-keys");
        std::fs::write(&path, "SET=\"value\"\nEMPTY=\"\"\n# COMMENTED=\"x\"\n").unwrap();
        let keys = read_keys(&path);
        assert_eq!(keys, HashSet::from(["SET".to_string()]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_keys_on_a_missing_file_is_empty() {
        let path = temp_path("read-keys-missing");
        assert!(read_keys(&path).is_empty());
    }

    #[test]
    fn validate_accepts_a_normal_key_and_value() {
        assert!(validate("RAINDROP_TOKEN", "abc").is_ok());
    }
}
