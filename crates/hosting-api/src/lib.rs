//! Hosting control-plane API.
//!
//! Reads tenant state from the on-disk layout produced by
//! `ops/tenant-provision.sh` (SIS) and wraps `tenant-backup.sh` behind an
//! authenticated-by-network-position HTTP surface. Designed to bind on
//! `127.0.0.1` inside the hosting VM and be exposed only through the edge
//! proxy with auth in front of it.
//!
//! # Endpoints
//!
//! | Method | Path | Purpose |
//! |--------|------|---------|
//! | GET | `/healthz` | liveness |
//! | GET | `/metrics` | Prometheus exposition |
//! | GET | `/api/tenants` | all tenants with live status |
//! | GET | `/api/tenants/{tenant}` | one tenant |
//! | POST | `/api/tenants/{tenant}/backup?offsite=true` | trigger backup |
//!
//! Secrets from `.credentials` are parsed server-side but never serialised.

pub mod auth;
pub mod backup;
pub mod error;
pub mod jobs;
pub mod nightly;
pub mod passkeys;
pub mod tenants;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics_kit::{Counter, Gauge, Registry};
use serde::Deserialize;
use webauthn_kit::{AuthenticationResponse, RegistrationResponse};

use crate::backup::BackupGate;
use crate::jobs::JobStore;
use crate::tenants::TenantRoot;

/// Shared handler state.
pub struct AppState {
    /// Tenant directory root.
    root: TenantRoot,
    /// Path to `tenant-backup.sh`.
    backup_script: PathBuf,
    /// Per-tenant backup mutual exclusion.
    pub gate: Arc<BackupGate>,
    /// Metrics registry (dogfood: `metrics-kit` via `telemetry-init`).
    registry: Arc<Registry>,
    /// Durable backup-job history.
    pub jobs: Arc<JobStore>,
    /// Bearer token for `/api/*` (`None` = API closed).
    pub api_token: Option<String>,
    /// Trust `X-Forwarded-User` from the edge proxy for audit identity.
    /// Only enable when the API is unreachable except through the edge.
    pub trust_proxy_headers: bool,
    /// Passkey ceremonies; `None` when passkeys are not configured.
    pub passkeys: Option<Arc<passkeys::PasskeyState>>,

    /// Total backup requests accepted.
    metric_backups_total: Counter,
    /// Backup failures (non-zero exit or error).
    metric_backup_failures: Counter,
    /// Current count of tenants considered healthy.
    metric_tenants_up: Gauge,
}

impl AppState {
    /// Build state from explicit parts (tests inject fixtures here).
    ///
    /// # Errors
    /// Propagates [`tenants::TenantRoot`] validation and metric
    /// registration errors.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tenant_root: PathBuf,
        backup_script: PathBuf,
        registry: Arc<Registry>,
        jobs_dir: PathBuf,
        api_token: Option<String>,
        trust_proxy_headers: bool,
    ) -> Result<Self, error::ApiError> {
        let root = TenantRoot::new(tenant_root)?;
        let metric_backups_total = registry
            .counter(
                "hosting_backups_total",
                "Tenant backup runs triggered via the API.",
                &[],
            )
            .map_err(|e| error::ApiError::Config(e.to_string()))?;
        let metric_backup_failures = registry
            .counter(
                "hosting_backup_failures_total",
                "Tenant backup runs that failed.",
                &[],
            )
            .map_err(|e| error::ApiError::Config(e.to_string()))?;
        let metric_tenants_up = registry
            .gauge(
                "hosting_tenants_up",
                "Tenants whose compose services are all up and healthy.",
                &[],
            )
            .map_err(|e| error::ApiError::Config(e.to_string()))?;
        let jobs = Arc::new(JobStore::open(jobs_dir)?);
        let passkeys = passkeys::PasskeyState::from_env()?;
        Ok(Self {
            root,
            backup_script,
            gate: Arc::new(BackupGate::default()),
            registry,
            jobs,
            api_token,
            trust_proxy_headers,
            passkeys,
            metric_backups_total,
            metric_backup_failures,
            metric_tenants_up,
        })
    }

    /// Tenant root path (for the nightly sweep's re-clone).
    #[must_use]
    pub fn tenant_root_path(&self) -> std::path::PathBuf {
        self.root.path().to_path_buf()
    }

    /// Backup script path (for the nightly sweep's re-clone).
    #[must_use]
    pub fn backup_script_path(&self) -> PathBuf {
        self.backup_script.clone()
    }

    /// Build the application router. `/api/*` requires the bearer
    /// token; `/healthz` and `/metrics` stay open.
    pub fn router(self: Arc<Self>) -> Router {
        // Passkey ceremonies are deliberately NOT behind the bearer
        // middleware: login must be reachable unauthenticated, and
        // registration enforces its own bootstrap rule inside the handler.
        let passkey_routes = Router::new()
            .route("/register/begin", post(pk_register_begin))
            .route("/register/finish", post(pk_register_finish))
            .route("/login/begin", post(pk_login_begin))
            .route("/login/finish", post(pk_login_finish))
            .route("/me", get(pk_me));

        let api = Router::new()
            .route("/tenants", get(list_tenants))
            .route("/tenants/{tenant}", get(get_tenant))
            .route("/tenants/{tenant}/backup", post(trigger_backup))
            .route("/jobs", get(list_jobs))
            .route("/jobs/{id}", get(get_job))
            .route("/tenants/{tenant}/jobs", get(list_tenant_jobs))
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self),
                auth::require_bearer,
            ));
        Router::new()
            .route("/healthz", get(healthz))
            .route("/metrics", get(metrics))
            .nest("/api/auth/passkey", passkey_routes)
            .nest("/api", api)
            .with_state(self)
    }
}

/// Query parameters for the backup trigger.
#[derive(Debug, Deserialize, Default)]
pub struct BackupQuery {
    /// Also copy the snapshot to the offsite (B2) repository.
    offsite: Option<bool>,
}

async fn healthz() -> &'static str {
    "ok"
}

/// Edge-forwarded identity for the audit trail. Only meaningful when
/// `trust_proxy_headers` is on; an unauthenticated caller able to reach
/// the API directly could forge this header, which is why the default is
/// off and the listener is loopback-only.
fn forwarded_user(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("X-Forwarded-User")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._@".contains(c))
        })
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    // Refresh the tenants-up gauge opportunistically; scrape-time cost is
    // one directory scan, acceptable at pilot scale.
    let tenants = state.root.all();
    let mut up = 0u64;
    for tenant in &tenants {
        if let Ok(creds) = state.root.credentials(tenant) {
            if state
                .root
                .status(tenant)
                .await
                .map(|s| s.healthy)
                .unwrap_or(false)
            {
                up += 1;
                tracing::debug!(tenant = %creds.tenant, domain = %creds.domain, "tenant healthy");
            }
        }
    }
    #[allow(clippy::cast_precision_loss)] // gauge values are f64 by design
    state
        .metric_tenants_up
        .set(f64::from(u32::try_from(up).unwrap_or(u32::MAX)));
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.registry.render(),
    )
        .into_response()
}

async fn list_tenants(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<tenants::Tenant>>, error::ApiError> {
    let mut out = Vec::new();
    for tenant in state.root.all() {
        if let (Ok(creds), Ok(status)) = (
            state.root.credentials(&tenant),
            state.root.status(&tenant).await,
        ) {
            out.push(tenants::Tenant {
                credentials: creds,
                status,
            });
        }
    }
    Ok(Json(out))
}

async fn get_tenant(
    State(state): State<Arc<AppState>>,
    Path(tenant): Path<String>,
) -> Result<Json<tenants::Tenant>, error::ApiError> {
    let creds = state.root.credentials(&tenant)?;
    let status = state.root.status(&tenant).await?;
    Ok(Json(tenants::Tenant {
        credentials: creds,
        status,
    }))
}

async fn trigger_backup(
    State(state): State<Arc<AppState>>,
    Path(tenant): Path<String>,
    Query(query): Query<BackupQuery>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<serde_json::Value>), error::ApiError> {
    // Validate the tenant before submitting so unknown slugs get a clean
    // 404 rather than a job that fails later.
    state.root.tenant_dir(&tenant)?;

    let offsite = query.offsite.unwrap_or(false);
    // Identity arrives via the edge proxy; only trusted when the process
    // was configured accordingly (loopback + edge-only exposure).
    let requested_by = if state.trust_proxy_headers {
        forwarded_user(&headers)
    } else {
        None
    };
    tracing::info!(tenant = %tenant, offsite, requested_by = ?requested_by, "backup requested");
    state.metric_backups_total.inc();

    match jobs::spawn_backup_job(
        &state.jobs,
        &state.gate,
        &state.backup_script,
        &tenant,
        offsite,
        requested_by,
    )
    .await
    {
        Ok(job_id) => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "job_id": job_id })),
        )),
        Err(e) => {
            state.metric_backup_failures.inc();
            tracing::error!(tenant = %tenant, error = %e, "backup submit failed");
            Err(e)
        }
    }
}

/// Passkey ceremonies return 503 when not configured.
fn pk_disabled() -> error::ApiError {
    error::ApiError::NotConfigured
}

async fn pk_register_begin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, error::ApiError> {
    let Some(pk) = state.passkeys.as_ref() else {
        return Err(pk_disabled());
    };
    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .filter(|u| {
            !u.is_empty()
                && u.len() <= 64
                && u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._@".contains(c))
        })
        .ok_or_else(|| error::ApiError::Config("invalid username".to_string()))?;
    // Bootstrap rule: the FIRST credential needs no auth (otherwise an
    // unconfigured platform could never be set up); later registrations
    // require the bearer token or an existing passkey session. These
    // routes sit outside require_bearer, so the check happens here.
    let bearer_ok = auth::bearer_matches(&state, &headers).await;
    let session = headers
        .get("X-Passkey-Session")
        .and_then(|v| v.to_str().ok());
    let session_user = match session {
        Some(t) => pk.session_user(t).await,
        None => None,
    };
    if !pk.may_register(bearer_ok, session_user.as_deref()).await {
        return Err(error::ApiError::Config(
            "registration requires an existing session (bootstrap credential already enrolled)"
                .to_string(),
        ));
    }
    Ok(Json(passkeys::register_begin(pk, username).await?))
}

async fn pk_register_finish(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, error::ApiError> {
    let Some(pk) = state.passkeys.as_ref() else {
        return Err(pk_disabled());
    };
    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let challenge_id = body
        .get("challenge_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let response: RegistrationResponse = serde_json::from_value(
        body.get("response")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| error::ApiError::Config(format!("bad response: {e}")))?;
    Ok(Json(
        passkeys::register_finish(pk, username, challenge_id, &response).await?,
    ))
}

async fn pk_login_begin(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, error::ApiError> {
    let Some(pk) = state.passkeys.as_ref() else {
        return Err(pk_disabled());
    };
    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    Ok(Json(passkeys::login_begin(pk, username).await?))
}

async fn pk_login_finish(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, error::ApiError> {
    let Some(pk) = state.passkeys.as_ref() else {
        return Err(pk_disabled());
    };
    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let challenge_id = body
        .get("challenge_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let response: AuthenticationResponse = serde_json::from_value(
        body.get("response")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| error::ApiError::Config(format!("bad response: {e}")))?;
    Ok(Json(
        passkeys::login_finish(pk, username, challenge_id, &response).await?,
    ))
}

async fn pk_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, error::ApiError> {
    let Some(pk) = state.passkeys.as_ref() else {
        return Err(pk_disabled());
    };
    let token = headers
        .get("X-Passkey-Session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let user = pk
        .session_user(token)
        .await
        .ok_or_else(|| error::ApiError::TenantNotFound("session expired".to_string()))?;
    Ok(Json(serde_json::json!({ "user": user })))
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Vec<jobs::JobRecord>> {
    Json(state.jobs.list(None, 100).await)
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<jobs::JobRecord>, error::ApiError> {
    state
        .jobs
        .get(&id)
        .await
        .map(Json)
        .ok_or_else(|| error::ApiError::TenantNotFound(format!("job {id}")))
}

async fn list_tenant_jobs(
    State(state): State<Arc<AppState>>,
    Path(tenant): Path<String>,
) -> Result<Json<Vec<jobs::JobRecord>>, error::ApiError> {
    state.root.tenant_dir(&tenant)?;
    Ok(Json(state.jobs.list(Some(&tenant), 100).await))
}
