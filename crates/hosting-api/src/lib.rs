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

pub mod backup;
pub mod error;
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
use crate::tenants::TenantRoot;

/// Shared handler state.
pub struct AppState {
    /// Tenant directory root.
    root: TenantRoot,
    /// Path to `tenant-backup.sh`.
    backup_script: PathBuf,
    /// Per-tenant backup mutual exclusion.
    gate: Arc<BackupGate>,
    /// Metrics registry (dogfood: `metrics-kit` via `telemetry-init`).
    registry: Arc<Registry>,
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
    pub fn new(
        tenant_root: PathBuf,
        backup_script: PathBuf,
        registry: Arc<Registry>,
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
        Ok(Self {
            root,
            backup_script,
            gate: Arc::new(BackupGate::default()),
            registry,
            metric_backups_total,
            metric_backup_failures,
            metric_tenants_up,
        })
    }

    /// Build the application router.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/metrics", get(metrics))
            .route("/api/tenants", get(list_tenants))
            .route("/api/tenants/{tenant}", get(get_tenant))
            .route("/api/tenants/{tenant}/backup", post(trigger_backup))
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
) -> Result<Json<backup::BackupOutcome>, error::ApiError> {
    // Validate the tenant before claiming the gate so unknown slugs get a
    // clean 404 rather than a gate slot.
    state.root.tenant_dir(&tenant)?;

    let offsite = query.offsite.unwrap_or(false);
    tracing::info!(tenant = %tenant, offsite, "backup requested");
    state.metric_backups_total.inc();

    match backup::run_backup(&state.gate, &state.backup_script, &tenant, offsite).await {
        Ok(outcome) => {
            if outcome.success {
                tracing::info!(tenant = %tenant, secs = outcome.duration_secs, "backup done");
            } else {
                state.metric_backup_failures.inc();
                tracing::warn!(tenant = %tenant, code = ?outcome.code, "backup script failed");
            }
            Ok(Json(outcome))
        }
        Err(e) => {
            state.metric_backup_failures.inc();
            tracing::error!(tenant = %tenant, error = %e, "backup error");
            Err(e)
        }
    }
}
