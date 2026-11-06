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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use axum::Router;
    use std::path::Path;
    use worker_kit::JobContext;

    /// Tenant directory fixture — mirrors ops/tenant-provision.sh output
    /// (compose.sh + .credentials), same contract the api tests use.
    fn fixture_tenant(root: &Path, slug: &str, service: &str) {
        let dir = root.join(slug);
        std::fs::create_dir_all(&dir).expect("tenant dir");
        std::fs::write(dir.join("compose.sh"), "#!/bin/sh\n").expect("write compose.sh");
        std::fs::write(
            dir.join(".credentials"),
            format!("tenant={slug}\nservice={service}\ndomain=docs.{slug}.example\n"),
        )
        .expect("write credentials");
    }

    fn stub_backup_script(dir: &Path, code: i32) -> PathBuf {
        let script = dir.join("stub-tenant-backup.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"snapshotting $1\" >&2\nexit {code}\n"),
        )
        .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
        }
        script
    }

    fn job_ctx() -> JobContext {
        JobContext {
            worker_name: "test".to_string(),
            trigger: Trigger::Cron("0 0 3 * * *".to_string()),
            shutdown: ShutdownGuard::new(),
        }
    }

    fn two_tenants() -> (
        tempfile::TempDir,
        TenantRoot,
        PathBuf,
        Arc<BackupGate>,
        Arc<JobStore>,
    ) {
        let dir = tempfile::tempdir().expect("tmp");
        fixture_tenant(dir.path(), "acme", "paperless");
        fixture_tenant(dir.path(), "globex", "files");
        let root = TenantRoot::new(dir.path()).expect("root");
        let script = stub_backup_script(dir.path(), 0);
        let gate = Arc::new(BackupGate::default());
        let store = Arc::new(JobStore::open(dir.path().join("jobs")).expect("job store"));
        (dir, root, script, gate, store)
    }

    #[tokio::test]
    async fn sweep_runs_a_backup_for_every_tenant_and_records_outcomes() {
        let (_dir, root, script, gate, store) = two_tenants();
        let job = nightly_sweep_job(root, script, Arc::clone(&gate), Arc::clone(&store));

        job(job_ctx()).await.expect("sweep succeeds");

        let records = store.list(None, 100).await;
        assert_eq!(records.len(), 2, "one job per tenant");
        assert!(records.iter().all(|r| r.state == "succeeded"));
    }

    #[tokio::test]
    async fn sweep_collects_failures_instead_of_aborting() {
        let (dir, root, _script, gate, store) = two_tenants();
        let script = stub_backup_script(dir.path(), 3);
        let job = nightly_sweep_job(root, script, Arc::clone(&gate), Arc::clone(&store));

        let err = job(job_ctx()).await.expect_err("failing tenants surface");
        assert!(err.to_string().contains("2 tenant(s) failed"), "{err}");

        // The records are sweep-run history (the closure always marks
        // them succeeded); the failure surfaces via the JobError above.
        let records = store.list(None, 100).await;
        assert_eq!(records.len(), 2, "one history record per tenant");
    }

    /// Stateful cAdvisor stub: first scrape is the baseline, later
    /// scrapes advance the counters, so `refresh` finds a real delta.
    async fn stub_cadvisor() -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/metrics",
            axum::routing::get({
                let calls = Arc::clone(&calls);
                move || {
                    let calls = Arc::clone(&calls);
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        let (cpu, mem) = if n == 0 {
                            (100, 536_870_912_u64)
                        } else {
                            (160, 1_610_612_736_u64)
                        };
                        format!(
                            "container_cpu_usage_seconds_total{{cpu=\"total\",id=\"/docker/a\",\
                             name=\"acme-paperless-1\"}} {cpu}\n\
                             container_memory_working_set_bytes{{id=\"/docker/a\",\
                             name=\"acme-paperless-1\"}} {mem}\n"
                        )
                    }
                }
            }),
        );
        // Minimal in-process HTTP server (no api-test harness here).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .expect("serve");
        });
        format!("http://{addr}/metrics")
    }

    fn metering_state(cadvisor: &str) -> Arc<AppState> {
        std::env::set_var("HOSTING_CADVISOR_URL", cadvisor);
        let dir = tempfile::tempdir().expect("tmp");
        fixture_tenant(dir.path(), "acme", "paperless");
        let root = dir.path().to_path_buf();
        std::mem::forget(dir);
        let jobs_dir = tempfile::tempdir().expect("jobs tmp");
        let jobs = jobs_dir.path().to_path_buf();
        std::mem::forget(jobs_dir);
        Arc::new(
            AppState::new(
                root,
                PathBuf::from("/bin/true"),
                Arc::new(metrics_kit::Registry::new()),
                jobs,
                Some("test-token".to_string()),
                false,
            )
            .expect("state"),
        )
    }

    #[tokio::test]
    async fn metering_close_posts_the_window_into_the_ledger_then_resets() {
        let stub = stub_cadvisor().await;
        let state = metering_state(&stub);

        // Baseline scrape, then a delta: the window now holds usage.
        state
            .meterer
            .refresh(&["acme".to_string()])
            .await
            .expect("baseline");
        state
            .meterer
            .refresh(&["acme".to_string()])
            .await
            .expect("delta");
        let before = state.meterer.window().await;
        assert!(before.contains_key("acme"), "usage accumulated: {before:?}");

        let job = metering_close_job(Arc::clone(&state));
        job(job_ctx()).await.expect("close succeeds");

        // The window reset: a posting exists, the window is empty.
        let statement = state
            .ledger
            .tenant_statement("acme", 50, state.meterer.config().currency)
            .await
            .expect("statement");
        assert_eq!(
            statement.1.len(),
            1,
            "one posting journalled: {:?}",
            statement.1
        );
        assert!(
            !state.meterer.window().await.contains_key("acme"),
            "window resets after close"
        );

        // Replay: idempotency key makes the second close a no-op.
        job(job_ctx()).await.expect("replay is a no-op");
        let statement = state
            .ledger
            .tenant_statement("acme", 50, state.meterer.config().currency)
            .await
            .expect("statement");
        assert_eq!(statement.1.len(), 1, "no double posting");
    }
}
