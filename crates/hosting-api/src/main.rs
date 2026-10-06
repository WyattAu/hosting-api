//! hosting-api entrypoint.
//!
//! Configuration via environment:
//! - `TENANT_ROOT` — tenant directory (default `/srv/tenants`)
//! - `HOSTING_LISTEN` — bind address (default `127.0.0.1:8484`)
//! - `HOSTING_BACKUP_SCRIPT` — path to `tenant-backup.sh`
//!   (default `/opt/sis-hosting/bin/tenant-backup.sh`)
//! - `RUST_LOG` — tracing filter (default `info`)
//!
//! Bind to loopback: this API has no auth of its own by design; the edge
//! proxy in front of it must authenticate. Exposing it directly is a
//! misconfiguration.

use std::path::PathBuf;
use std::sync::Arc;

use hosting_api::AppState;

/// Default tenant root mirrors the provisioner's `TENANT_ROOT`.
const DEFAULT_TENANT_ROOT: &str = "/srv/tenants";
/// Default listen address: loopback only.
const DEFAULT_LISTEN: &str = "127.0.0.1:8484";
/// Default script path mirrors the Ansible role's install target.
const DEFAULT_BACKUP_SCRIPT: &str = "/opt/sis-hosting/bin/tenant-backup.sh";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let telemetry = telemetry_init::Telemetry::init(
        telemetry_init::TelemetryConfig::new("hosting-api")
            .version(env!("CARGO_PKG_VERSION"))
            .log_format(telemetry_init::LogFormat::Json)
            .log_level("info"),
    )
    .unwrap_or_else(|e| {
        eprintln!("telemetry init failed: {e}");
        std::process::exit(1);
    });

    let tenant_root = PathBuf::from(env_or("TENANT_ROOT", DEFAULT_TENANT_ROOT));
    let listen = env_or("HOSTING_LISTEN", DEFAULT_LISTEN);
    let backup_script = PathBuf::from(env_or("HOSTING_BACKUP_SCRIPT", DEFAULT_BACKUP_SCRIPT));

    let state = match AppState::new(
        tenant_root.clone(),
        backup_script.clone(),
        telemetry.metrics(),
    ) {
        Ok(state) => Arc::new(state),
        Err(e) => {
            tracing::error!(error = %e, "invalid configuration");
            let _ = telemetry.shutdown();
            std::process::exit(1);
        }
    };

    let app = state.router();

    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%listen, error = %e, "failed to bind");
            let _ = telemetry.shutdown();
            std::process::exit(1);
        }
    };

    tracing::info!(
        %listen,
        tenant_root = %tenant_root.display(),
        backup_script = %backup_script.display(),
        version = env!("CARGO_PKG_VERSION"),
        "hosting-api listening"
    );

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
    };

    if let Err(e) = axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!(error = %e, "server error");
    }

    if let Err(e) = telemetry.shutdown() {
        tracing::warn!(error = %e, "telemetry shutdown incomplete");
    }
}
