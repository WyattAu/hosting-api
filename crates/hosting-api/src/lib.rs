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
pub mod tenants;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics_kit::{Counter, Gauge, Registry};
use serde::Deserialize;

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
    pub fn new(
        tenant_root: PathBuf,
        backup_script: PathBuf,
        registry: Arc<Registry>,
        jobs_dir: PathBuf,
        api_token: Option<String>,
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
        Ok(Self {
            root,
            backup_script,
            gate: Arc::new(BackupGate::default()),
            registry,
            jobs,
            api_token,
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
    query: Query<BackupQuery>,
) -> Result<(StatusCode, Json<serde_json::Value>), error::ApiError> {
    // Validate the tenant before submitting so unknown slugs get a clean
    // 404 rather than a job that fails later.
    state.root.tenant_dir(&tenant)?;

    let offsite = query.offsite.unwrap_or(false);
    tracing::info!(tenant = %tenant, offsite, "backup requested");
    state.metric_backups_total.inc();

    match jobs::spawn_backup_job(
        &state.jobs,
        &state.gate,
        &state.backup_script,
        &tenant,
        offsite,
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
