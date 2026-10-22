//! hosting-api entrypoint.
//!
//! Configuration via environment:
//! - `TENANT_ROOT` — tenant directory (default `/srv/tenants`)
//! - `HOSTING_LISTEN` — bind address (default `127.0.0.1:8484`)
//! - `HOSTING_BACKUP_SCRIPT` — path to `tenant-backup.sh`
//!   (default `/opt/sis-hosting/bin/tenant-backup.sh`)
//! - `HOSTING_JOBS_DIR` — durable job history (default `/srv/backups/jobs`)
//! - `HOSTING_API_TOKEN` — bearer token for `/api/*` (required; empty
//!   closes the API)
//! - `HOSTING_PASSKEY_RP_ID` / `HOSTING_PASSKEY_ORIGIN` — enable passkey
//!   step-up ceremonies when both are set (off by default)
//! - `HOSTING_PASSKEYS_FILE` — credential store (default
//!   `/etc/sis-hosting/passkeys.json`)
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
/// Default durable job-history directory.
const DEFAULT_JOBS_DIR: &str = "/srv/backups/jobs";

/// Layered configuration via the estate's `envstack`: host environment
/// first, defaults filled from the constants below.
fn load_config() -> Config {
    let stack = envstack::ConfigStack::new().with_env();
    let get = |key: &str| -> Option<String> {
        stack
            .get(key)
            .and_then(|v| v.as_str().map(ToOwned::to_owned))
    };
    let require = |key: &str| -> Option<String> { get(key).filter(|v| !v.is_empty()) };
    // NOTE (SIS round 6, iter 51): envstack lowercases env-var names when
    // it builds its layer tree, and `ConfigStack::get` resolves paths
    // case-sensitively — so config keys are queried LOWERCASE here. The
    // uppercase spellings silently resolved to None (shipped 0.5.0 bug:
    // the API booted with HOSTING_API_TOKEN ignored; caught by the real-
    // binary verification this iteration, see SIS .docs/engineering-loop.md).
    Config {
        tenant_root: get("tenant_root").unwrap_or_else(|| DEFAULT_TENANT_ROOT.to_string()),
        listen: get("hosting_listen").unwrap_or_else(|| DEFAULT_LISTEN.to_string()),
        backup_script: get("hosting_backup_script")
            .unwrap_or_else(|| DEFAULT_BACKUP_SCRIPT.to_string()),
        jobs_dir: get("hosting_jobs_dir").unwrap_or_else(|| DEFAULT_JOBS_DIR.to_string()),
        api_token: require("hosting_api_token"),
        trust_proxy_headers: get("trust_proxy_headers")
            .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1"),
    }
}

/// Process configuration, resolved once at startup.
struct Config {
    tenant_root: String,
    listen: String,
    backup_script: String,
    jobs_dir: String,
    api_token: Option<String>,
    trust_proxy_headers: bool,
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

    let cfg = load_config();
    let tenant_root = PathBuf::from(&cfg.tenant_root);
    let listen = cfg.listen.clone();
    let backup_script = PathBuf::from(&cfg.backup_script);
    let jobs_dir = PathBuf::from(&cfg.jobs_dir);
    let api_token = cfg.api_token.clone();
    let trust_proxy_headers = cfg.trust_proxy_headers;

    if api_token.is_none() {
        tracing::warn!("HOSTING_API_TOKEN not set — /api/* is CLOSED (fail closed)");
    }

    let state = match AppState::new(
        tenant_root.clone(),
        backup_script.clone(),
        telemetry.metrics(),
        jobs_dir,
        api_token,
        trust_proxy_headers,
    ) {
        Ok(state) => Arc::new(state),
        Err(e) => {
            tracing::error!(error = %e, "invalid configuration");
            let _ = telemetry.shutdown();
            std::process::exit(1);
        }
    };

    // Mark jobs orphaned by a previous process as failed.
    state.jobs.reap_orphans().await;

    // Recurring work on the estate's worker-kit (cron sweep, breaker,
    // graceful drain). The supervisor parks until the shutdown guard is
    // triggered; run it alongside the HTTP server.
    let guard = hosting_api::nightly::shutdown_guard();
    let mut supervisor = worker_kit::WorkerSupervisor::new(guard.clone());
    if let Err(e) = hosting_api::nightly::register_nightly(&mut supervisor, &state) {
        tracing::error!(error = %e, "failed to register nightly sweep");
        let _ = telemetry.shutdown();
        std::process::exit(1);
    }
    if let Err(e) = hosting_api::nightly::register_metering_close(&mut supervisor, &state) {
        tracing::error!(error = %e, "failed to register metering close");
        let _ = telemetry.shutdown();
        std::process::exit(1);
    }
    // Warm the OIDC JWKS cache at boot (round 7 iter 64): an issuer
    // that is unreachable at startup must not wedge the boot — the
    // validator re-fetches on demand; this just makes the first request
    // cheap and surfaces misconfiguration early in the logs.
    if let Some(oidc) = state.oidc.as_ref() {
        let oidc = std::sync::Arc::clone(oidc);
        tokio::spawn(async move {
            if let Err(e) = oidc.refresh_jwks().await {
                tracing::warn!(error = %e, "OIDC JWKS warm-up failed (will retry on demand)");
            }
        });
    }

    // Replay the ledger outbox into the fast journal (round 6 iter 51):
    // at-least-once, idempotent on posting ids — a crash between post and
    // replay loses nothing.
    {
        let ledger = std::sync::Arc::clone(&state.ledger);
        tokio::spawn(async move {
            match ledger.restore().await {
                Ok(n) if n > 0 => tracing::info!(restored = n, "ledger outbox replayed"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "ledger outbox replay failed"),
            }
        });
    }
    let supervisor = Arc::new(supervisor);
    let supervisor_handle = tokio::spawn(Arc::clone(&supervisor).run());

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

    let shutdown = {
        let guard = guard.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
            guard.shutdown(); // starts the supervisor's graceful drain
        }
    };

    if let Err(e) = axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!(error = %e, "server error");
    }

    // Wait for the supervisor's drain report, then flush telemetry.
    if let Ok(report) = supervisor_handle.await {
        tracing::info!(?report, "supervisor drained");
    }
    if let Err(e) = telemetry.shutdown() {
        tracing::warn!(error = %e, "telemetry shutdown incomplete");
    }
}
