//! Typed errors for the hosting control plane.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Errors surfaced by the API layer.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// A tenant slug did not match the expected shape, or the tenant
    /// directory does not exist under the configured [`crate::tenants::TenantRoot`].
    #[error("tenant not found or invalid: {0}")]
    TenantNotFound(String),

    /// A subprocess (compose ps, backup script) failed or timed out.
    #[error("subprocess failure: {0}")]
    Subprocess(String),

    /// An upstream dependency (cAdvisor) could not be scraped.
    #[error("upstream failure: {0}")]
    Upstream(String),

    /// Filesystem access failed (missing permissions, vanished directory).
    #[error("io failure: {0}")]
    Io(#[from] std::io::Error),

    /// Metering configuration was invalid.
    #[error("usage failure: {0}")]
    Usage(#[from] crate::usage::UsageError),

    /// A backup is already running for this tenant.
    #[error("backup already in progress for tenant {0}")]
    BackupInProgress(String),

    /// The platform is misconfigured (missing binary, bad root path).
    #[error("configuration error: {0}")]
    Config(String),

    /// A capability is intentionally not configured on this deployment
    /// (e.g. passkeys without `HOSTING_PASSKEY_*` env).
    #[error("not configured on this deployment")]
    NotConfigured,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::TenantNotFound(_) | ApiError::BackupInProgress(_) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            ApiError::Subprocess(_) => (StatusCode::BAD_GATEWAY, "subprocess_failed"),
            ApiError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_failed"),
            ApiError::Usage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "usage_error"),
            ApiError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "io_error"),
            ApiError::NotConfigured => (StatusCode::SERVICE_UNAVAILABLE, "not_configured"),
            ApiError::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "config_error"),
        };
        // BackupInProgress is really 409; adjust before responding.
        let status = if matches!(self, ApiError::BackupInProgress(_)) {
            StatusCode::CONFLICT
        } else {
            status
        };
        let body = axum::Json(json!({
            "error": { "code": code, "message": self.to_string() }
        }));
        (status, body).into_response()
    }
}
