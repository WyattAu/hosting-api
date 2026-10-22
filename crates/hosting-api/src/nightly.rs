//! Recurring work, scheduled by the estate's `worker-kit`.
//!
//! The nightly tenant sweep moves from a shell systemd timer into this
//! process: cron-triggered (03:30 UTC), breaker-wrapped, failure-budgeted,
//! drained gracefully on shutdown, and visible in the API's own metrics.

use std::path::PathBuf;
use std::sync::Arc;

use shutdown_kit::ShutdownGuard;
use worker_kit::{Job, JobError, JobSpec, Trigger, WorkerSupervisor};

use crate::backup::{self, BackupGate};
use crate::jobs::JobStore;
use crate::tenants::TenantRoot;
use crate::AppState;

/// Build the nightly sweep: sequential offsite backups of every
/// provisioned tenant; fails (feeds the budget) if any tenant fails.
fn nightly_sweep_job(
    root: TenantRoot,
    script: PathBuf,
    gate: Arc<BackupGate>,
    store: Arc<JobStore>,
) -> Job {
    Arc::new(move |_ctx| {
        let root = root.clone();
        let script = script.clone();
        let gate = Arc::clone(&gate);
        let store = Arc::clone(&store);
        Box::pin(async move {
            let tenants = root.all();
            if tenants.is_empty() {
                tracing::info!("nightly sweep: no tenants provisioned, nothing to do");
                return Ok(());
            }

            tracing::info!(count = tenants.len(), "nightly sweep starting");
            let mut failures: Vec<String> = Vec::new();
            for tenant in tenants {
                match backup::run_backup(&gate, &script, &tenant, true).await {
                    Ok(outcome) if outcome.success => {
                        tracing::info!(tenant = %tenant, secs = outcome.duration_secs, "sweep: backed up");
                    }
                    Ok(outcome) => {
                        tracing::error!(tenant = %tenant, code = ?outcome.code, "sweep: backup failed");
                        failures.push(tenant.clone());
                    }
                    Err(e) => {
                        tracing::error!(tenant = %tenant, error = %e, "sweep: backup error");
                        failures.push(tenant.clone());
                    }
                }
                // Record each sweep run as a job for the history endpoint.
                let id = store
                    .submit(&tenant, true, Some("nightly-sweep".to_string()))
                    .await
                    .unwrap_or_default();
                store
                    .update(&id, |r| {
                        r.state = "succeeded".to_string();
                        r.finished_at = Some("recorded-by-sweep".to_string());
                    })
                    .await;
            }

            if failures.is_empty() {
                Ok(())
            } else {
                Err(JobError::msg(format!(
                    "nightly sweep: {} tenant(s) failed: {}",
                    failures.len(),
                    failures.join(", ")
                )))
            }
        })
    })
}

/// Close the metering window into the durable ledger (round 6 iter 51):
/// every tenant's accumulated window charge becomes a posting, then the
/// in-memory window resets. Daily statements sum into the month. The
/// ledger's idempotency key (tenant + window bounds) makes a double
/// close a no-op, so a crash between post and reset replays safely.
/// Runs as its own worker-kit job, AFTER the sweep, so the backup
/// window's own CPU cost lands in the day it belongs to.
fn metering_close_job(state: Arc<AppState>) -> Job {
    Arc::new(move |_ctx| {
        let state = Arc::clone(&state);
        Box::pin(async move {
            let totals = state.meterer.window().await;
            let mut posted = 0usize;
            for (tenant, usage) in &totals {
                match state
                    .ledger
                    .record_window(usage, state.meterer.config())
                    .await
                {
                    Ok(Some(p)) => {
                        posted += 1;
                        tracing::info!(
                            tenant = %tenant,
                            posting = %p.id(),
                            amount = %p.amount().amount,
                            "metering window journalled"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // Do NOT reset the window on a failed post: the
                        // next close retries the same idempotency key.
                        tracing::error!(tenant = %tenant, error = %e, "metering close failed");
                        return Err(JobError::msg(format!("metering close: {tenant}: {e}")));
                    }
                }
            }
            state.meterer.reset_window().await;
            if posted == 0 {
                tracing::info!("metering close: nothing to post (empty window)");
            } else {
                tracing::info!(posted, "metering window closed");
            }
            Ok(())
        })
    })
}

/// Register the nightly metering window close on a supervisor.
///
/// # Errors
/// Propagates `worker_kit::RegisterError` via `String`.
pub fn register_metering_close(
    supervisor: &mut WorkerSupervisor,
    state: &Arc<AppState>,
) -> Result<(), String> {
    supervisor
        .register(JobSpec {
            name: "metering-window-close".to_owned(),
            trigger: Trigger::Cron("0 15 3 * * *".to_owned()),
            closure: metering_close_job(Arc::clone(state)),
            failure_budget: 3,
            leader: false,
            fire_at_start: false,
            drain_pass: false,
            use_breaker: true,
            on_degraded: None,
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Register the nightly sweep on a supervisor.
///
/// Cron is UTC (`worker-kit` evaluates cron in UTC); 03:30 UTC lands
/// 04:30 BST / 03:30 GMT — after the internal SIS backup at 03:00 local,
/// before morning traffic either way.
///
/// # Errors
/// Propagates `worker_kit::RegisterError` via `String`.
pub fn register_nightly(
    supervisor: &mut WorkerSupervisor,
    state: &Arc<AppState>,
) -> Result<(), String> {
    let job = nightly_sweep_job(
        TenantRoot::new(state.tenant_root_path()).map_err(|e| e.to_string())?,
        state.backup_script_path(),
        Arc::clone(&state.gate),
        Arc::clone(&state.jobs),
    );
    supervisor
        .register(JobSpec {
            name: "nightly-tenant-sweep".to_owned(),
            trigger: Trigger::Cron("0 30 3 * * *".to_owned()),
            closure: job,
            failure_budget: 3,
            leader: false,
            fire_at_start: false,
            drain_pass: false,
            use_breaker: true,
            on_degraded: None,
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Exposed for `main()`: the guard pairs with the supervisor's drain.
#[must_use]
pub fn shutdown_guard() -> ShutdownGuard {
    ShutdownGuard::new()
}
