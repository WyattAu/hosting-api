//! Passkey (WebAuthn/FIDO2) step-up authentication — the estate's
//! `webauthn-kit` wired to a file-backed credential store.
//!
//! Scope of this spike (recorded in the SIS engineering loop):
//! - Registration/authentication ceremonies for `/api/*` step-up use.
//! - First credential bootstraps with no bearer token required; every
//!   subsequent registration requires an existing session or the bearer
//!   token (otherwise anyone on the network could enrol a passkey).
//! - Sessions are in-memory with a TTL — restart logs users out, which
//!   for a control plane is a feature.
//!
//! Enabled only when `HOSTING_PASSKEY_RP_ID` and `HOSTING_PASSKEY_ORIGIN`
//! are set; `/api/auth/passkey/*` answers 503 otherwise.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use webauthn_kit::{
    verify_authentication, verify_registration, AuthenticationParams, AuthenticationResponse,
    ChallengeStore, CredentialPolicy, RegistrationResponse, UserVerificationPolicy, WebauthnConfig,
    WebauthnCredential,
};

use crate::error::ApiError;

/// Session lifetime: a working day on the control plane.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Everything `WebAuthn` needs, present only when enabled.
pub struct PasskeyState {
    /// Public configuration for the RP.
    pub config: WebauthnConfig,
    /// Pending challenges (single-use, TTL handled by the kit).
    pub challenges: Mutex<ChallengeStore>,
    /// Persisted credentials.
    pub store: PasskeyStore,
    /// Live sessions (token → (username, expiry)).
    pub sessions: Mutex<HashMap<String, (String, Instant)>>,
}

impl PasskeyState {
    /// Build from environment; `None` when disabled.
    ///
    /// # Errors
    /// [`ApiError::Config`] when enabled but the credential file cannot be
    /// opened.
    pub fn from_env() -> Result<Option<Arc<Self>>, ApiError> {
        let rp_id = std::env::var("HOSTING_PASSKEY_RP_ID").unwrap_or_default();
        let origin = std::env::var("HOSTING_PASSKEY_ORIGIN").unwrap_or_default();
        if rp_id.is_empty() || origin.is_empty() {
            return Ok(None);
        }
        let file = std::env::var("HOSTING_PASSKEYS_FILE")
            .unwrap_or_else(|_| "/etc/sis-hosting/passkeys.json".to_string());
        let store = PasskeyStore::open(PathBuf::from(&file))?;
        let config = WebauthnConfig {
            rp_id: rp_id.clone(),
            rp_name: "WyattAu Hosting Control Plane".to_string(),
            rp_origins: vec![origin],
            allowed_algorithms: vec![-7, -257], // ES256, RS256
            challenge_timeout_secs: 300,
            attestation: webauthn_kit::AttestationPolicy::default(),
            credential_policy: CredentialPolicy {
                user_verification: UserVerificationPolicy::Required,
                ..CredentialPolicy::default()
            },
            resident_key: webauthn_kit::ResidentKeyPolicy::default(),
            attestation_conveyance: webauthn_kit::AttestationConveyance::default(),
        };
        Ok(Some(Arc::new(Self {
            config,
            challenges: Mutex::new(ChallengeStore::new()),
            store,
            sessions: Mutex::new(HashMap::new()),
        })))
    }

    /// Issue a session token for a verified user.
    pub async fn issue_session(&self, username: &str) -> String {
        let token = crate::jobs::new_job_id(); // 32 hex chars of CSPRNG-ish id
        self.sessions.lock().await.insert(
            token.clone(),
            (username.to_string(), Instant::now() + SESSION_TTL),
        );
        token
    }

    /// Validate a session token, returning the username.
    pub async fn session_user(&self, token: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().await;
        let now = Instant::now();
        sessions.retain(|_, (_, exp)| *exp > now);
        sessions.get(token).map(|(u, _)| u.clone())
    }

    /// Resolve an identity from a passkey session header. Returns None
    /// unless the token maps to a live session.
    pub async fn identity_from_header(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        let token = headers
            .get("X-Passkey-Session")
            .and_then(|v| v.to_str().ok())?;
        self.session_user(token).await
    }

    /// Registration guard: bootstrap rule — the first credential needs no
    /// existing session; later ones do.
    pub async fn may_register(&self, bearer_ok: bool, session: Option<&str>) -> bool {
        if session.is_some() || bearer_ok {
            return true;
        }
        self.store.is_empty().await
    }
}

/// File-backed credential store: username → credentials.
pub struct PasskeyStore {
    creds: Mutex<HashMap<String, Vec<WebauthnCredential>>>,
    path: PathBuf,
}

impl PasskeyStore {
    /// # Errors
    /// [`ApiError::Config`] when the file exists but cannot be parsed.
    pub fn open(path: PathBuf) -> Result<Self, ApiError> {
        let mut creds = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            creds = serde_json::from_str(&text)
                .map_err(|e| ApiError::Config(format!("passkey store {}: {e}", path.display())))?;
        }
        Ok(Self {
            creds: Mutex::new(creds),
            path,
        })
    }

    fn persist(&self, creds: &HashMap<String, Vec<WebauthnCredential>>) {
        if let Ok(json) = serde_json::to_string_pretty(creds) {
            let tmp = self.path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    /// True when no credentials are enrolled at all (bootstrap state).
    pub async fn is_empty(&self) -> bool {
        self.creds.lock().await.values().all(Vec::is_empty)
    }

    /// Credential IDs for a user (for the login allow-list).
    pub async fn ids_for(&self, username: &str) -> Vec<String> {
        self.creds
            .lock()
            .await
            .get(username)
            .map(|v| v.iter().map(|c| c.credential_id.clone()).collect())
            .unwrap_or_default()
    }

    /// Persist a newly registered credential.
    pub async fn add(&self, username: &str, cred: WebauthnCredential) {
        let mut creds = self.creds.lock().await;
        creds.entry(username.to_string()).or_default().push(cred);
        self.persist(&creds);
    }

    /// Look up a credential by ID, updating `last_used_at` on a hit.
    pub async fn find_and_touch(&self, credential_id: &str) -> Option<WebauthnCredential> {
        let mut creds = self.creds.lock().await;
        let mut found = None;
        for user_creds in creds.values_mut() {
            if let Some(c) = user_creds
                .iter_mut()
                .find(|c| c.credential_id == credential_id)
            {
                c.last_used_at = now_secs();
                found = Some(c.clone());
                break;
            }
        }
        if found.is_some() {
            self.persist(&creds);
        }
        found
    }

    /// Update the sign count after a verified authentication.
    pub async fn update_sign_count(&self, credential_id: &str, sign_count: u32) {
        let mut creds = self.creds.lock().await;
        let mut touched = false;
        for user_creds in creds.values_mut() {
            if let Some(c) = user_creds
                .iter_mut()
                .find(|c| c.credential_id == credential_id)
            {
                c.sign_count = sign_count;
                touched = true;
                break;
            }
        }
        if touched {
            self.persist(&creds);
        }
    }
}

fn now_secs() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Registration ceremony, step 1: issue `create()` options.
///
/// # Errors
/// [`ApiError::Config`] when passkeys are disabled.
pub async fn register_begin(
    state: &PasskeyState,
    username: &str,
) -> Result<serde_json::Value, ApiError> {
    let challenges = state.challenges.lock().await;
    let existing = state.store.ids_for(username).await;
    let (challenge_b64, options) =
        challenges.generate_registration_challenge(&state.config, username, username, &existing);
    Ok(serde_json::json!({ "challenge_id": challenge_b64, "options": options }))
}

/// Registration ceremony, step 2: verify and persist.
///
/// # Errors
/// [`ApiError::Config`] for challenge/verification failures.
pub async fn register_finish(
    state: &PasskeyState,
    username: &str,
    challenge_id: &str,
    response: &RegistrationResponse,
) -> Result<serde_json::Value, ApiError> {
    let mut challenges = state.challenges.lock().await;
    // consume returns (username, challenge_bytes) with TTL enforcement.
    let (challenge_user, challenge_bytes) = challenges
        .consume_registration_challenge(challenge_id, 300)
        .map_err(|e| ApiError::Config(format!("challenge: {e}")))?;
    if challenge_user != username {
        return Err(ApiError::Config("challenge username mismatch".to_string()));
    }

    let result = verify_registration(
        &challenge_bytes,
        &response.client_data_json,
        &response.attestation_object,
        "",
        &state.config.rp_id,
        &state.config.rp_origins,
        &state.config.attestation,
        &state.config.credential_policy,
    )
    .map_err(|e| ApiError::Config(format!("registration rejected: {e}")))?;

    // webauthn-kit 0.4 returns the attested COSE key directly (PR #9) —
    // the ~40-line consumer-side extraction from iteration 5 is gone.
    let cred = WebauthnCredential {
        credential_id: result.credential_id,
        public_key_cose: result.public_key_cose,
        sign_count: 0,
        device_name: result.device_name,
        registered_at: now_secs(),
        last_used_at: now_secs(),
        attestation_format: result.attestation_format,
        user_verified: result.user_verified,
        backup_eligible: result.backup_eligible,
        backup_state: result.backup_state,
    };
    state.store.add(username, cred.clone()).await;
    Ok(serde_json::json!({
        "credential_id": cred.credential_id,
        "attestation_trust": format!("{:?}", result.attestation.trust_level),
    }))
}

/// Authentication ceremony, step 1: issue `get()` options.
///
/// # Errors
/// [`ApiError::TenantNotFound`] for unknown users with no discoverable
/// credentials; [`ApiError::Config`] when disabled.
pub async fn login_begin(
    state: &PasskeyState,
    username: &str,
) -> Result<serde_json::Value, ApiError> {
    let challenges = state.challenges.lock().await;
    let ids = state.store.ids_for(username).await;
    let (challenge_b64, options) = challenges.generate_authentication_challenge(&state.config, ids);
    Ok(serde_json::json!({ "challenge_id": challenge_b64, "options": options }))
}

/// Authentication ceremony, step 2: verify assertion, mint a session.
///
/// # Errors
/// [`ApiError::TenantNotFound`] for unknown credentials;
/// [`ApiError::Config`] for ceremony failures.
pub async fn login_finish(
    state: &PasskeyState,
    username: &str,
    challenge_id: &str,
    response: &AuthenticationResponse,
) -> Result<serde_json::Value, ApiError> {
    let mut challenges = state.challenges.lock().await;
    let (challenge_user, challenge_bytes, _allowed) = challenges
        .consume_authentication_challenge(challenge_id, 300)
        .map_err(|e| ApiError::Config(format!("challenge: {e}")))?;
    if challenge_user != username {
        return Err(ApiError::Config("challenge username mismatch".to_string()));
    }

    let stored = state
        .store
        .find_and_touch(&response.id)
        .await
        .ok_or_else(|| ApiError::TenantNotFound(format!("credential {}", &response.id[..8])))?;

    let params = AuthenticationParams {
        challenge_bytes,
        client_data_json_b64: response.client_data_json.clone(),
        authenticator_data_b64: response.authenticator_data.clone(),
        signature_b64: response.signature.clone(),
        credential_id_b64: response.id.clone(),
        public_key_cose: stored.public_key_cose.clone(),
        current_sign_count: stored.sign_count,
        allowed_credential_ids: vec![response.id.clone()],
        rp_id: state.config.rp_id.clone(),
        rp_origins: state.config.rp_origins.clone(),
        policy: state.config.credential_policy.clone(),
    };
    let result = verify_authentication(&params)
        .map_err(|e| ApiError::Config(format!("authentication rejected: {e}")))?;

    // AuthenticationResult::new_sign_count is u32 (kit computes it from
    // the deferred-update policy); persist unconditionally.
    state
        .store
        .update_sign_count(&response.id, result.new_sign_count)
        .await;

    let token = state.issue_session(username).await;
    Ok(serde_json::json!({ "session": token, "username": username }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn store() -> PasskeyStore {
        let dir = tempfile::tempdir().expect("tmp");
        PasskeyStore::open(dir.path().join("p.json")).expect("open")
    }

    fn cred(id: &str) -> WebauthnCredential {
        WebauthnCredential {
            credential_id: id.to_string(),
            public_key_cose: vec![1, 2, 3],
            sign_count: 0,
            device_name: "test".to_string(),
            registered_at: 0,
            last_used_at: 0,
            attestation_format: "none".to_string(),
            user_verified: true,
            backup_eligible: false,
            backup_state: false,
        }
    }

    #[tokio::test]
    async fn bootstrap_empty_then_enrolled() {
        let s = store();
        assert!(s.is_empty().await, "fresh store bootstraps");
        s.add("wyatt", cred("abc123")).await;
        assert!(!s.is_empty().await, "enrolled store no longer bootstraps");
    }

    #[tokio::test]
    async fn find_and_touch_updates_last_used() {
        let s = store();
        s.add("wyatt", cred("abc123")).await;
        let found = s.find_and_touch("abc123").await.expect("found");
        assert!(found.last_used_at >= found.registered_at);
        assert!(s.find_and_touch("nope").await.is_none());
    }

    #[tokio::test]
    async fn sign_count_updates_persist() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = PasskeyStore::open(dir.path().join("p.json")).expect("open");
        s.add("wyatt", cred("abc123")).await;
        s.update_sign_count("abc123", 42).await;
        drop(s);
        let reopened = PasskeyStore::open(dir.path().join("p.json")).expect("reopen");
        let c = reopened.find_and_touch("abc123").await.expect("found");
        assert_eq!(c.sign_count, 42);
    }

    #[tokio::test]
    async fn sessions_expire_and_validate() {
        // Constructed directly (from_env requires env vars).
        let dir = tempfile::tempdir().expect("tmp");
        let state = PasskeyState {
            config: WebauthnConfig {
                rp_id: "localhost".to_string(),
                rp_name: "t".to_string(),
                rp_origins: vec!["http://localhost:8484".to_string()],
                allowed_algorithms: vec![-7],
                challenge_timeout_secs: 300,
                attestation: webauthn_kit::AttestationPolicy::default(),
                credential_policy: CredentialPolicy {
                    user_verification: UserVerificationPolicy::Required,
                    ..CredentialPolicy::default()
                },
                resident_key: webauthn_kit::ResidentKeyPolicy::default(),
                attestation_conveyance: webauthn_kit::AttestationConveyance::default(),
            },
            challenges: Mutex::new(ChallengeStore::new()),
            store: PasskeyStore::open(dir.path().join("p.json")).expect("open"),
            sessions: Mutex::new(HashMap::new()),
        };
        let token = state.issue_session("wyatt").await;
        assert_eq!(state.session_user(&token).await.as_deref(), Some("wyatt"));
        assert_eq!(state.session_user("bogus").await, None);
    }
}
