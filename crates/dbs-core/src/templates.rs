//! Embedded scaffolding templates written by `dbs init`.
//!
//! Mirrors `src/dbs/templates.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`) plus the writer half of `cli.py`'s `init`
//! command (that file has no `templates.py` counterpart of its own —
//! the reference keeps the two constants and the writer in separate
//! modules, and this port keeps that split conceptually while landing
//! both here since there's no CLI crate yet to host the writer).
//!
//! [`write_scaffolding`] is idempotent by design, matching the
//! reference: an existing config is left alone unless `force` is set
//! (never silently clobbered), and `.env.example` is never overwritten
//! at all — regenerating it would erase filled-in local secrets sitting
//! next to it in spirit (the file itself holds no secrets, but a user
//! editing it in place shouldn't have edits blown away by a second
//! `dbs init`).

use std::path::{Path, PathBuf};

use crate::errors::DbsError;

pub const CONFIG_TEMPLATE: &str = include_str!("templates/config.toml.template");
pub const ENV_TEMPLATE: &str = include_str!("templates/env.template");

fn io_err(e: std::io::Error) -> DbsError {
    DbsError::Storage(format!("failed to write scaffolding: {e}"))
}

/// What [`write_scaffolding`] actually did — both fields are `false`
/// when everything already existed and `force` wasn't set (a no-op
/// re-run reports that rather than erroring).
#[derive(Debug, Clone, PartialEq)]
pub struct ScaffoldResult {
    pub config_path: PathBuf,
    pub config_written: bool,
    pub env_example_path: PathBuf,
    pub env_example_written: bool,
}

/// Writes `config_path` (from [`CONFIG_TEMPLATE`]) unless it already
/// exists and `force` is `false`, and `<config_path's dir>/.env.example`
/// (from [`ENV_TEMPLATE`]) unless it already exists — `.env.example`
/// has no `force` override, matching the reference exactly.
pub fn write_scaffolding(config_path: &Path, force: bool) -> Result<ScaffoldResult, DbsError> {
    if let Some(parent) = config_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
    }

    let config_written = if !config_path.exists() || force {
        std::fs::write(config_path, CONFIG_TEMPLATE).map_err(io_err)?;
        true
    } else {
        false
    };

    let env_example_path = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(".env.example"))
        .unwrap_or_else(|| PathBuf::from(".env.example"));
    let env_example_written = if !env_example_path.exists() {
        std::fs::write(&env_example_path, ENV_TEMPLATE).map_err(io_err)?;
        true
    } else {
        false
    };

    Ok(ScaffoldResult {
        config_path: config_path.to_path_buf(),
        config_written,
        env_example_path,
        env_example_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dbs-templates-test-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_directory_writes_both_files() {
        let dir = temp_dir("fresh");
        let config_path = dir.join("dbs.toml");

        let result = write_scaffolding(&config_path, false).unwrap();
        assert!(result.config_written);
        assert!(result.env_example_written);
        assert!(config_path.is_file());
        assert!(dir.join(".env.example").is_file());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn already_initialized_directory_does_not_clobber_without_force() {
        let dir = temp_dir("no-clobber");
        let config_path = dir.join("dbs.toml");
        std::fs::write(&config_path, "# user's own config\n").unwrap();
        std::fs::write(dir.join(".env.example"), "# user's own env\n").unwrap();

        let result = write_scaffolding(&config_path, false).unwrap();
        assert!(!result.config_written);
        assert!(!result.env_example_written);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "# user's own config\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".env.example")).unwrap(),
            "# user's own env\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn force_overwrites_an_existing_config_but_never_the_env_example() {
        let dir = temp_dir("force");
        let config_path = dir.join("dbs.toml");
        std::fs::write(&config_path, "# stale config\n").unwrap();
        std::fs::write(dir.join(".env.example"), "# user's own env\n").unwrap();

        let result = write_scaffolding(&config_path, true).unwrap();
        assert!(result.config_written);
        assert!(!result.env_example_written);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            CONFIG_TEMPLATE
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".env.example")).unwrap(),
            "# user's own env\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_template_contains_every_expected_section() {
        assert!(CONFIG_TEMPLATE.contains("[dbs]"));
        assert!(CONFIG_TEMPLATE.contains("[sources.raindrop]"));
        assert!(CONFIG_TEMPLATE.contains("database ="));
        assert!(CONFIG_TEMPLATE.contains("token_env = \"RAINDROP_TOKEN\""));
    }

    #[test]
    fn env_template_documents_the_raindrop_token() {
        assert!(ENV_TEMPLATE.contains("RAINDROP_TOKEN="));
    }

    #[test]
    fn written_config_parses_as_valid_toml() {
        let dir = temp_dir("parses");
        let config_path = dir.join("dbs.toml");
        write_scaffolding(&config_path, false).unwrap();
        let text = std::fs::read_to_string(&config_path).unwrap();
        toml::from_str::<toml::Value>(&text).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
