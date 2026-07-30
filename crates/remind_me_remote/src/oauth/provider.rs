//! Single-user OAuth 2.1 provider (FT-07, `#86`) — the policy layer above
//! [`remind_me_core::remote::OAuthStateStore`]: consent, code/token
//! issuance, refresh rotation, and revocation.
//!
//! Ported from the reference's `SingleUserOAuthProvider`
//! (`remind_me_mcp/oauth.py`). There are no accounts or sessions: the
//! `/authorize` endpoint parks the request and sends the user-agent to
//! `/consent`, where the owner pastes the FT-05 connector token (repurposed
//! as the "owner credential") to approve the requesting client. A wrong
//! credential and an explicit deny produce the identical outcome (`routes.rs`
//! builds the same `access_denied` redirect for both) so the form never
//! leaks which part failed.
//!
//! Authorization codes and pending consent transactions are deliberately
//! process-local (`Mutex`-guarded `HashMap`s, not the JSON state file):
//! both legs of each exchange hit the same running process, and losing them
//! on restart only means re-running `/authorize` — unlike client
//! registrations and issued tokens, which must survive a restart and be
//! visible to `remind_me_revoke_clients` running in the separate stdio
//! process, and so live in [`remind_me_core::remote::OAuthStateStore`]
//! instead.
//!
//! Every method here is synchronous — pure computation plus small local
//! JSON-file I/O, no network calls. `routes.rs`'s handlers run
//! store-touching calls through `tokio::task::spawn_blocking`, mirroring
//! the reference's own `asyncio.to_thread` calls in `tools/admin.py`'s
//! `remind_me_revoke_clients` (PF-06 conventions) and this crate's own
//! established precedent in `handler.rs`.
//!
//! Expiry checks take `now: i64` explicitly rather than reading a global
//! clock, so tests can pass an arbitrary timestamp directly instead of
//! needing a mockable clock singleton (the reference's `_now()`, monkey-
//! patched in its test suite).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use remind_me_core::remote::{generate_token, OAuthStateStore, OAuthTokenCounts, TokenKind};
use remind_me_core::webhook::constant_time_eq;
use serde_json::{json, Value};

use super::types::{ClientInformation, ClientMetadata, TokenResponse};

/// Access tokens are short-lived — refresh is cheap and revocation windows
/// stay small.
pub const ACCESS_TOKEN_TTL: i64 = 3600;
/// Refresh tokens live 30 days and rotate on every refresh grant.
pub const REFRESH_TOKEN_TTL: i64 = 30 * 24 * 3600;
/// Authorization codes expire after 5 minutes and are single-use (consumed
/// only on a *successful* exchange — see [`Provider::exchange_authorization_code`]).
pub const AUTH_CODE_TTL: i64 = 300;
/// A pending consent page is valid for 10 minutes, then the txn expires.
pub const CONSENT_TTL: i64 = 600;
/// Where the owner-credential consent form lives (the `/authorize` redirect
/// target).
pub const CONSENT_PATH: &str = "/consent";
/// Synthetic client_id reported for requests authenticated with the legacy
/// connector token. Never collides with a registered client (those get
/// generated ids of a different, longer shape).
pub const OWNER_CLIENT_ID: &str = "owner";

/// Current UNIX time as `i64` seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parameters captured at `/authorize` time and carried through consent to
/// the issued authorization code — the reference's `AuthorizationParams`.
#[derive(Debug, Clone)]
pub struct AuthorizationParams {
    pub state: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub code_challenge: String,
    pub redirect_uri: String,
    pub redirect_uri_provided_explicitly: bool,
    pub resource: Option<String>,
}

#[derive(Debug, Clone)]
struct IssuedAuthorizationCode {
    scopes: Vec<String>,
    expires_at: i64,
    client_id: String,
    code_challenge: String,
    redirect_uri: String,
    redirect_uri_provided_explicitly: bool,
    resource: Option<String>,
}

/// A loaded authorization code, as returned to `routes.rs` for validation
/// before it decides whether to consume it.
#[derive(Debug, Clone)]
pub struct AuthorizationCodeView {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub expires_at: i64,
    pub code_challenge: String,
    pub redirect_uri: String,
    pub redirect_uri_provided_explicitly: bool,
}

#[derive(Debug, Clone)]
struct PendingConsent {
    client_id: String,
    params: AuthorizationParams,
    expires_at: i64,
}

/// What `/consent`'s GET renders — enough to show "X wants access" and the
/// redirect target.
#[derive(Debug, Clone)]
pub struct PendingConsentView {
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uri: String,
}

/// The result of a `/consent` POST decision.
pub enum ConsentOutcome {
    /// Unknown or expired txn — render the "expired" page.
    Expired,
    /// Approved: an authorization code was issued.
    Approved {
        code: String,
        redirect_uri: String,
        state: Option<String>,
    },
    /// Denied (wrong credential or explicit deny — indistinguishable on
    /// purpose).
    Denied {
        redirect_uri: String,
        state: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AccessTokenView {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct RefreshTokenView {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<i64>,
}

/// Why registering a client was refused (RFC 7591 §3.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationError {
    pub error: &'static str,
    pub error_description: String,
}

/// Single-user OAuth 2.1 provider (FT-07) over [`OAuthStateStore`].
pub struct Provider {
    owner_token: String,
    store: OAuthStateStore,
    codes: Mutex<HashMap<String, IssuedAuthorizationCode>>,
    pending: Mutex<HashMap<String, PendingConsent>>,
}

impl Provider {
    pub fn new(owner_token: String, store: OAuthStateStore) -> Self {
        Self {
            owner_token,
            store,
            codes: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &OAuthStateStore {
        &self.store
    }

    // -- client registry (RFC 7591) -----------------------------------------

    /// Load a registered client from the state file.
    pub fn get_client(&self, client_id: &str) -> Option<ClientInformation> {
        let record = self.store.get_client(client_id)?;
        serde_json::from_value(record).ok()
    }

    /// Persist a dynamically-registered client.
    ///
    /// Forces `token_endpoint_auth_method = "none"` (no `client_secret`),
    /// overriding whatever the client requested — the reference's own
    /// `register_client` doc explains why at length: unlike access/refresh
    /// tokens (SHA-256 hashed at rest), a `client_secret_post`/`basic`
    /// client's secret would have to be compared byte-for-byte against
    /// whatever the client presents, so storing only a hash isn't an
    /// option, and PKCE (S256, mandatory below) already provides proof of
    /// possession without one. Because every registered client always gets
    /// `client_secret = None`, `routes.rs`'s client authentication for
    /// `/token` and `/revoke` never needs to compare a secret at all — see
    /// this crate's ADR for that specific, deliberate simplification.
    pub fn register_client(
        &self,
        metadata: ClientMetadata,
    ) -> Result<ClientInformation, RegistrationError> {
        if metadata.redirect_uris.is_empty() {
            return Err(RegistrationError {
                error: "invalid_client_metadata",
                error_description: "redirect_uris must contain at least one URI".to_string(),
            });
        }
        if !metadata
            .grant_types
            .iter()
            .any(|g| g == "authorization_code")
            || !metadata.grant_types.iter().any(|g| g == "refresh_token")
        {
            return Err(RegistrationError {
                error: "invalid_client_metadata",
                error_description: "grant_types must be authorization_code and refresh_token"
                    .to_string(),
            });
        }
        if !metadata.response_types.iter().any(|r| r == "code") {
            return Err(RegistrationError {
                error: "invalid_client_metadata",
                error_description:
                    "response_types must include 'code' for authorization_code grant".to_string(),
            });
        }

        let client_id = generate_token();
        let info = ClientInformation {
            client_id: Some(client_id.clone()),
            client_secret: None,
            client_id_issued_at: Some(now_unix()),
            client_secret_expires_at: None,
            redirect_uris: metadata.redirect_uris,
            token_endpoint_auth_method: "none".to_string(),
            grant_types: metadata.grant_types,
            response_types: metadata.response_types,
            scope: metadata.scope,
            client_name: metadata.client_name,
            extra: metadata.extra,
        };
        let record = serde_json::to_value(&info).unwrap_or_else(|_| json!({}));
        self.store.put_client(&client_id, record);
        Ok(info)
    }

    // -- authorization + consent ---------------------------------------------

    /// Park the request and return the `/consent?txn=...` path the caller
    /// should redirect to.
    pub fn authorize(&self, client_id: &str, params: AuthorizationParams, now: i64) -> String {
        self.prune_pending(now);
        let txn = generate_token();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.insert(
            txn.clone(),
            PendingConsent {
                client_id: client_id.to_string(),
                params,
                expires_at: now + CONSENT_TTL,
            },
        );
        format!("{CONSENT_PATH}?txn={txn}")
    }

    fn prune_pending(&self, now: i64) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, p| p.expires_at >= now);
    }

    /// GET `/consent`: look up a live txn without consuming it, plus the
    /// client's display name for the approval form.
    pub fn pending_consent(&self, txn: &str, now: i64) -> Option<PendingConsentView> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let entry = pending.get(txn)?;
        if entry.expires_at < now {
            return None;
        }
        let client_name = self
            .get_client(&entry.client_id)
            .and_then(|c| c.client_name);
        Some(PendingConsentView {
            client_id: entry.client_id.clone(),
            client_name,
            redirect_uri: entry.params.redirect_uri.clone(),
        })
    }

    /// POST `/consent`: consume the txn (single-use whatever the outcome)
    /// and either issue a code (approved) or report the denial redirect.
    /// `approved` has already folded together "wrong owner credential" and
    /// "explicit deny" — see [`Self::verify_owner_token`].
    pub fn decide_consent(&self, txn: &str, approved: bool, now: i64) -> ConsentOutcome {
        let entry = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(txn)
        };
        let Some(entry) = entry else {
            return ConsentOutcome::Expired;
        };
        if entry.expires_at < now {
            return ConsentOutcome::Expired;
        }
        if !approved {
            return ConsentOutcome::Denied {
                redirect_uri: entry.params.redirect_uri,
                state: entry.params.state,
            };
        }
        let code = generate_token();
        let issued = IssuedAuthorizationCode {
            scopes: entry.params.scopes.clone().unwrap_or_default(),
            expires_at: now + AUTH_CODE_TTL,
            client_id: entry.client_id,
            code_challenge: entry.params.code_challenge.clone(),
            redirect_uri: entry.params.redirect_uri.clone(),
            redirect_uri_provided_explicitly: entry.params.redirect_uri_provided_explicitly,
            resource: entry.params.resource.clone(),
        };
        let mut codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
        codes.insert(code.clone(), issued);
        ConsentOutcome::Approved {
            code,
            redirect_uri: entry.params.redirect_uri,
            state: entry.params.state,
        }
    }

    /// Constant-time compare against the owner credential. A missing or
    /// wrong value and an explicit "deny" action both end up `false` in the
    /// caller (`routes.rs`), producing the identical `access_denied`
    /// redirect — the trust boundary never explains itself.
    pub fn verify_owner_token(&self, presented: &str) -> bool {
        constant_time_eq(presented.as_bytes(), self.owner_token.as_bytes())
    }

    // -- authorization-code exchange -----------------------------------------

    /// Look up an issued code without consuming it, so `routes.rs` can run
    /// expiry / redirect_uri / PKCE checks before deciding whether to
    /// consume it. A code whose `client_id` doesn't match reads as absent —
    /// matching the reference's "if code belongs to different client,
    /// pretend it doesn't exist".
    pub fn load_authorization_code(
        &self,
        client_id: &str,
        code: &str,
    ) -> Option<AuthorizationCodeView> {
        let codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
        let entry = codes.get(code)?;
        if entry.client_id != client_id {
            return None;
        }
        Some(AuthorizationCodeView {
            client_id: entry.client_id.clone(),
            scopes: entry.scopes.clone(),
            expires_at: entry.expires_at,
            code_challenge: entry.code_challenge.clone(),
            redirect_uri: entry.redirect_uri.clone(),
            redirect_uri_provided_explicitly: entry.redirect_uri_provided_explicitly,
        })
    }

    /// Consume the (single-use) code and issue a fresh access + refresh
    /// token pair. Only reached after `routes.rs` has already validated
    /// expiry, redirect_uri, and PKCE via [`Self::load_authorization_code`]
    /// — a failed validation deliberately does *not* consume the code
    /// (matches the reference: `exchange_authorization_code` is a separate
    /// step from `load_authorization_code`, only called once every prior
    /// check passed).
    pub fn exchange_authorization_code(&self, code: &str, now: i64) -> Option<TokenResponse> {
        let entry = {
            let mut codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
            codes.remove(code)?
        };
        Some(self.issue_tokens(&entry.client_id, entry.scopes, entry.resource, now))
    }

    // -- refresh grant --------------------------------------------------------

    /// Look up a refresh token by hash; expired or foreign tokens read as
    /// absent (an expired one is also evicted from the store).
    pub fn load_refresh_token(
        &self,
        client_id: &str,
        token: &str,
        now: i64,
    ) -> Option<RefreshTokenView> {
        let meta = self.store.get_token(TokenKind::Refresh, token)?;
        if meta.get("client_id").and_then(Value::as_str) != Some(client_id) {
            return None;
        }
        let expires_at = meta.get("expires_at").and_then(Value::as_i64);
        if let Some(exp) = expires_at {
            if exp < now {
                self.store.delete_token(TokenKind::Refresh, token);
                return None;
            }
        }
        Some(RefreshTokenView {
            client_id: client_id.to_string(),
            scopes: scopes_of(&meta),
            expires_at,
        })
    }

    /// Rotate: retire the presented refresh token, issue a fresh pair.
    pub fn exchange_refresh_token(
        &self,
        client_id: &str,
        token: &str,
        scopes: Vec<String>,
        now: i64,
    ) -> TokenResponse {
        self.store.delete_token(TokenKind::Refresh, token);
        self.issue_tokens(client_id, scopes, None, now)
    }

    // -- bearer verification --------------------------------------------------

    /// Verify a bearer token: an issued OAuth access token OR the legacy
    /// connector token. The legacy acceptance is what keeps the FT-05
    /// secret-path URL and header-capable bearer clients working while
    /// OAuth is active — both funnel through the same
    /// [`crate::oauth::routes::require_bearer`] middleware.
    pub fn load_access_token(&self, token: &str, now: i64) -> Option<AccessTokenView> {
        if self.verify_owner_token(token) {
            return Some(AccessTokenView {
                client_id: OWNER_CLIENT_ID.to_string(),
                scopes: Vec::new(),
                expires_at: None,
            });
        }
        let meta = self.store.get_token(TokenKind::Access, token)?;
        let expires_at = meta.get("expires_at").and_then(Value::as_i64);
        if let Some(exp) = expires_at {
            if exp < now {
                self.store.delete_token(TokenKind::Access, token);
                return None;
            }
        }
        let client_id = meta.get("client_id").and_then(Value::as_str)?.to_string();
        Some(AccessTokenView {
            client_id,
            scopes: scopes_of(&meta),
            expires_at,
        })
    }

    // -- revocation (RFC 7009) -------------------------------------------------

    /// Revoke every token of `client_id` — the RFC's SHOULD, applied
    /// unconditionally here: presenting either half of a client's
    /// credential pair kills every token that client holds (the
    /// registration itself survives, so the client can re-authorize).
    pub fn revoke_tokens_for_client(&self, client_id: &str) -> OAuthTokenCounts {
        self.store.delete_tokens_for_client(client_id)
    }

    // -- issuance ---------------------------------------------------------------

    fn issue_tokens(
        &self,
        client_id: &str,
        scopes: Vec<String>,
        resource: Option<String>,
        now: i64,
    ) -> TokenResponse {
        let access_token = generate_token();
        let refresh_token = generate_token();
        self.store.put_token(
            TokenKind::Access,
            &access_token,
            json!({
                "client_id": client_id,
                "scopes": scopes,
                "expires_at": now + ACCESS_TOKEN_TTL,
                "resource": resource,
            }),
        );
        self.store.put_token(
            TokenKind::Refresh,
            &refresh_token,
            json!({
                "client_id": client_id,
                "scopes": scopes,
                "expires_at": now + REFRESH_TOKEN_TTL,
            }),
        );
        TokenResponse {
            access_token,
            token_type: "Bearer",
            expires_in: Some(ACCESS_TOKEN_TTL),
            scope: if scopes.is_empty() {
                None
            } else {
                Some(scopes.join(" "))
            },
            refresh_token: Some(refresh_token),
        }
    }
}

fn scopes_of(meta: &Value) -> Vec<String> {
    meta.get("scopes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_store(label: &str) -> OAuthStateStore {
        let dir = std::env::temp_dir().join(format!(
            "rrm_oauth_provider_{label}_{}_{}",
            std::process::id(),
            generate_token()
        ));
        OAuthStateStore::new(dir.join("oauth.json"))
    }

    fn cleanup(store: &OAuthStateStore) {
        if let Some(parent) = store.path().parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn params(challenge: &str) -> AuthorizationParams {
        AuthorizationParams {
            state: Some("st4te".to_string()),
            scopes: None,
            code_challenge: challenge.to_string(),
            redirect_uri: "https://claude.ai/cb".to_string(),
            redirect_uri_provided_explicitly: true,
            resource: None,
        }
    }

    fn register(provider: &Provider) -> ClientInformation {
        provider
            .register_client(ClientMetadata {
                redirect_uris: vec!["https://claude.ai/cb".to_string()],
                token_endpoint_auth_method: Some("client_secret_post".to_string()),
                grant_types: default_grant_types(),
                response_types: default_response_types(),
                scope: None,
                client_name: Some("claude.ai".to_string()),
                extra: Default::default(),
            })
            .unwrap()
    }

    fn default_grant_types() -> Vec<String> {
        vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ]
    }
    fn default_response_types() -> Vec<String> {
        vec!["code".to_string()]
    }

    #[test]
    fn registering_a_client_always_forces_none_auth_and_no_secret() {
        let store = temp_store("register");
        let path = store.path().to_path_buf();
        let provider = Provider::new("owner-token".to_string(), store);

        let info = register(&provider);
        assert_eq!(info.token_endpoint_auth_method, "none");
        assert!(info.client_secret.is_none());
        assert!(info.client_secret_expires_at.is_none());

        cleanup(&OAuthStateStore::new(PathBuf::from(path.parent().unwrap())));
    }

    #[test]
    fn registration_rejects_missing_redirect_uris_and_wrong_grant_or_response_types() {
        let store = temp_store("register_reject");
        let provider = Provider::new("owner-token".to_string(), store);

        assert!(provider
            .register_client(ClientMetadata {
                redirect_uris: vec![],
                token_endpoint_auth_method: None,
                grant_types: default_grant_types(),
                response_types: default_response_types(),
                scope: None,
                client_name: None,
                extra: Default::default(),
            })
            .is_err());

        assert!(provider
            .register_client(ClientMetadata {
                redirect_uris: vec!["https://a.example/cb".to_string()],
                token_endpoint_auth_method: None,
                grant_types: vec!["authorization_code".to_string()],
                response_types: default_response_types(),
                scope: None,
                client_name: None,
                extra: Default::default(),
            })
            .is_err());

        assert!(provider
            .register_client(ClientMetadata {
                redirect_uris: vec!["https://a.example/cb".to_string()],
                token_endpoint_auth_method: None,
                grant_types: default_grant_types(),
                response_types: vec!["token".to_string()],
                scope: None,
                client_name: None,
                extra: Default::default(),
            })
            .is_err());
    }

    #[test]
    fn full_authorize_consent_code_exchange_flow_issues_working_tokens() {
        let store = temp_store("flow");
        let provider = Provider::new("owner-token".to_string(), store);
        let info = register(&provider);
        let client_id = info.client_id.clone().unwrap();
        let now = now_unix();

        let redirect_path = provider.authorize(&client_id, params("challenge-abc"), now);
        let txn = redirect_path
            .strip_prefix("/consent?txn=")
            .unwrap()
            .to_string();

        let view = provider.pending_consent(&txn, now).unwrap();
        assert_eq!(view.client_id, client_id);
        assert_eq!(view.client_name.as_deref(), Some("claude.ai"));

        let outcome = provider.decide_consent(&txn, true, now);
        let code = match outcome {
            ConsentOutcome::Approved {
                code,
                redirect_uri,
                state,
            } => {
                assert_eq!(redirect_uri, "https://claude.ai/cb");
                assert_eq!(state.as_deref(), Some("st4te"));
                code
            }
            _ => panic!("expected Approved"),
        };

        // The txn is single-use -- a second decision on it is Expired.
        assert!(matches!(
            provider.decide_consent(&txn, true, now),
            ConsentOutcome::Expired
        ));

        let loaded = provider.load_authorization_code(&client_id, &code).unwrap();
        assert_eq!(loaded.code_challenge, "challenge-abc");

        let tokens = provider.exchange_authorization_code(&code, now).unwrap();
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_in, Some(ACCESS_TOKEN_TTL));
        assert!(tokens.refresh_token.is_some());

        // Single-use: the same code cannot be loaded (let alone exchanged) again.
        assert!(provider
            .load_authorization_code(&client_id, &code)
            .is_none());

        let access = provider
            .load_access_token(&tokens.access_token, now)
            .unwrap();
        assert_eq!(access.client_id, client_id);

        cleanup(provider.store());
    }

    #[test]
    fn denied_consent_and_a_wrong_owner_credential_both_report_denied() {
        let store = temp_store("deny");
        let provider = Provider::new("owner-token".to_string(), store);
        let info = register(&provider);
        let client_id = info.client_id.clone().unwrap();
        let now = now_unix();

        assert!(!provider.verify_owner_token("not-the-owner-token"));

        let redirect_path = provider.authorize(&client_id, params("challenge"), now);
        let txn = redirect_path
            .strip_prefix("/consent?txn=")
            .unwrap()
            .to_string();
        let outcome = provider.decide_consent(&txn, false, now);
        assert!(matches!(outcome, ConsentOutcome::Denied { .. }));

        cleanup(provider.store());
    }

    #[test]
    fn expired_pending_consent_reports_expired_not_the_stale_client() {
        let store = temp_store("expired_pending");
        let provider = Provider::new("owner-token".to_string(), store);
        let info = register(&provider);
        let client_id = info.client_id.clone().unwrap();
        let now = now_unix();

        let redirect_path = provider.authorize(&client_id, params("challenge"), now);
        let txn = redirect_path
            .strip_prefix("/consent?txn=")
            .unwrap()
            .to_string();

        let far_future = now + CONSENT_TTL + 1;
        assert!(provider.pending_consent(&txn, far_future).is_none());
        assert!(matches!(
            provider.decide_consent(&txn, true, far_future),
            ConsentOutcome::Expired
        ));

        cleanup(provider.store());
    }

    #[test]
    fn refresh_grant_rotates_the_refresh_token_and_expired_access_tokens_stop_authenticating() {
        let store = temp_store("refresh");
        let provider = Provider::new("owner-token".to_string(), store);
        let info = register(&provider);
        let client_id = info.client_id.clone().unwrap();
        let now = now_unix();

        let redirect_path = provider.authorize(&client_id, params("challenge"), now);
        let txn = redirect_path
            .strip_prefix("/consent?txn=")
            .unwrap()
            .to_string();
        let code = match provider.decide_consent(&txn, true, now) {
            ConsentOutcome::Approved { code, .. } => code,
            _ => panic!("expected Approved"),
        };
        let tokens = provider.exchange_authorization_code(&code, now).unwrap();
        let old_refresh = tokens.refresh_token.unwrap();

        let refresh_view = provider
            .load_refresh_token(&client_id, &old_refresh, now)
            .unwrap();
        assert_eq!(refresh_view.client_id, client_id);

        let rotated = provider.exchange_refresh_token(&client_id, &old_refresh, vec![], now);
        assert_ne!(rotated.access_token, tokens.access_token);
        assert_ne!(rotated.refresh_token.as_ref().unwrap(), &old_refresh);

        // Old refresh token is dead (rotation).
        assert!(provider
            .load_refresh_token(&client_id, &old_refresh, now)
            .is_none());

        // The original access token expires after its TTL.
        assert!(provider
            .load_access_token(&tokens.access_token, now)
            .is_some());
        assert!(provider
            .load_access_token(&tokens.access_token, now + ACCESS_TOKEN_TTL + 1)
            .is_none());

        cleanup(provider.store());
    }

    #[test]
    fn the_legacy_owner_token_authenticates_as_the_synthetic_owner_client() {
        let store = temp_store("legacy");
        let provider = Provider::new("owner-token".to_string(), store);
        let view = provider
            .load_access_token("owner-token", now_unix())
            .unwrap();
        assert_eq!(view.client_id, OWNER_CLIENT_ID);
        assert!(provider
            .load_access_token("wrong-token", now_unix())
            .is_none());
        cleanup(provider.store());
    }

    #[test]
    fn revoking_a_clients_tokens_kills_both_access_and_refresh_but_leaves_other_clients_alone() {
        let store = temp_store("revoke");
        let provider = Provider::new("owner-token".to_string(), store);
        let a = register(&provider);
        let a_id = a.client_id.clone().unwrap();
        let now = now_unix();

        let redirect_path = provider.authorize(&a_id, params("challenge"), now);
        let txn = redirect_path
            .strip_prefix("/consent?txn=")
            .unwrap()
            .to_string();
        let code = match provider.decide_consent(&txn, true, now) {
            ConsentOutcome::Approved { code, .. } => code,
            _ => panic!("expected Approved"),
        };
        let tokens = provider.exchange_authorization_code(&code, now).unwrap();

        let counts = provider.revoke_tokens_for_client(&a_id);
        assert_eq!(counts.access_tokens, 1);
        assert_eq!(counts.refresh_tokens, 1);
        assert!(provider
            .load_access_token(&tokens.access_token, now)
            .is_none());
        assert!(provider
            .load_refresh_token(&a_id, &tokens.refresh_token.unwrap(), now)
            .is_none());

        cleanup(provider.store());
    }
}
