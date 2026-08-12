//! Scoped secret access for connectors (least privilege).
//!
//! Mirrors `src/dbs/core/secrets.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). A connector declares `secret_keys`; the engine
//! hands it a [`Secrets`] accessor scoped to *only* those keys, so a
//! connector cannot read another connector's tokens even though they all
//! live in the same environment. (Once ADR-0001's subprocess boundary
//! lands, this becomes an even harder guarantee — a subprocess literally
//! cannot read a secret it wasn't handed on stdin — but the in-process
//! accessor here is still the shape connector-internal code is written
//! against.)

use std::collections::HashMap;

use crate::errors::ConnectorError;

/// A read-only, allow-listed view over a secret store.
#[derive(Debug, Clone)]
pub struct Secrets {
    store: HashMap<String, String>,
    allowed: Vec<String>,
}

impl Secrets {
    /// `store` is typically a snapshot of the process environment. `allowed`
    /// is the connector's declared `secret_keys`.
    pub fn new(store: HashMap<String, String>, allowed: Vec<String>) -> Self {
        Self { store, allowed }
    }

    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// Returns the secret for `key`.
    ///
    /// Errors with [`ConnectorError::Contract`] if `key` wasn't declared in
    /// `secret_keys`, or [`ConnectorError::Auth`] if it was declared but is
    /// missing/empty in the store.
    pub fn get(&self, key: &str) -> Result<&str, ConnectorError> {
        if !self.allowed.iter().any(|k| k == key) {
            return Err(ConnectorError::Contract(format!(
                "secret {key:?} was not declared in this connector's secret_keys {:?}; declare it to access it.",
                self.allowed
            )));
        }
        match self.store.get(key) {
            Some(value) if !value.is_empty() => Ok(value.as_str()),
            _ => Err(ConnectorError::Auth(format!(
                "required secret {key:?} is not set in the environment."
            ))),
        }
    }

    /// Like [`Self::get`] but returns `default` if the (declared) key is
    /// unset, instead of erroring.
    pub fn get_optional<'a>(
        &'a self,
        key: &str,
        default: Option<&'a str>,
    ) -> Result<Option<&'a str>, ConnectorError> {
        if !self.allowed.iter().any(|k| k == key) {
            return Err(ConnectorError::Contract(format!(
                "secret {key:?} was not declared in this connector's secret_keys {:?}.",
                self.allowed
            )));
        }
        match self.store.get(key) {
            Some(value) if !value.is_empty() => Ok(Some(value.as_str())),
            _ => Ok(default),
        }
    }

    /// Pre-flight: errors with [`ConnectorError::Auth`] listing every
    /// missing key, if any.
    pub fn require_all(&self) -> Result<(), ConnectorError> {
        let missing: Vec<&str> = self
            .allowed
            .iter()
            .filter(|k| self.store.get(*k).is_none_or(|v| v.is_empty()))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConnectorError::Auth(format!(
                "missing required secret(s): {}",
                missing.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets_with(pairs: &[(&str, &str)], allowed: &[&str]) -> Secrets {
        let store = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let allowed = allowed.iter().map(|s| s.to_string()).collect();
        Secrets::new(store, allowed)
    }

    #[test]
    fn get_returns_declared_present_value() {
        let secrets = secrets_with(&[("RAINDROP_TOKEN", "abc123")], &["RAINDROP_TOKEN"]);
        assert_eq!(secrets.get("RAINDROP_TOKEN").unwrap(), "abc123");
    }

    #[test]
    fn get_rejects_undeclared_key_as_contract_error() {
        let secrets = secrets_with(&[("OTHER_TOKEN", "xyz")], &["RAINDROP_TOKEN"]);
        let err = secrets.get("OTHER_TOKEN").unwrap_err();
        assert!(matches!(err, ConnectorError::Contract(_)));
    }

    #[test]
    fn get_rejects_declared_but_missing_key_as_auth_error() {
        let secrets = secrets_with(&[], &["RAINDROP_TOKEN"]);
        let err = secrets.get("RAINDROP_TOKEN").unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn get_rejects_declared_but_empty_key_as_auth_error() {
        let secrets = secrets_with(&[("RAINDROP_TOKEN", "")], &["RAINDROP_TOKEN"]);
        let err = secrets.get("RAINDROP_TOKEN").unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn get_optional_falls_back_to_default_when_unset() {
        let secrets = secrets_with(&[], &["OPTIONAL_KEY"]);
        assert_eq!(
            secrets
                .get_optional("OPTIONAL_KEY", Some("fallback"))
                .unwrap(),
            Some("fallback")
        );
    }

    #[test]
    fn get_optional_still_rejects_undeclared_key() {
        let secrets = secrets_with(&[], &["OTHER_KEY"]);
        assert!(secrets.get_optional("UNDECLARED", None).is_err());
    }

    #[test]
    fn require_all_lists_every_missing_key() {
        let secrets = secrets_with(&[("A", "1")], &["A", "B", "C"]);
        let err = secrets.require_all().unwrap_err();
        match err {
            ConnectorError::Auth(msg) => {
                assert!(msg.contains('B'));
                assert!(msg.contains('C'));
                assert!(!msg.contains("A,") && !msg.starts_with('A'));
            }
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    #[test]
    fn require_all_passes_when_everything_present() {
        let secrets = secrets_with(&[("A", "1"), ("B", "2")], &["A", "B"]);
        assert!(secrets.require_all().is_ok());
    }
}
