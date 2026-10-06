//! Backup orchestration: thin, auditable wrapper over the provisioner's
//! `tenant-backup.sh`.
//!
//! The script is the source of truth (it stops the stack, snapshots with
//! restic, restarts, and optionally copies offsite). This module adds
//! what an API needs around it: per-tenant mutual exclusion, a hard
//! timeout, and captured stderr for the caller.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::Command;

/// Default wall-clock budget for one tenant backup (compose stop + restic
/// snapshot + B2 copy for a mid-size library).
const BACKUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Serialises backups per tenant.
///
/// Uses a std mutex because critical sections are single set operations —
/// never held across an await — which keeps [`BackupClaim::drop`] trivially
/// synchronous and correct.
#[derive(Debug, Default)]
pub struct BackupGate {
    running: Mutex<HashSet<String>>,
}

impl BackupGate {
    /// Attempt to claim the backup slot for `tenant`.
    ///
    /// # Errors
    /// [`crate::error::ApiError::BackupInProgress`] when the tenant is
    /// already being backed up; [`crate::error::ApiError::Config`] if the
    /// internal lock was poisoned by a panic in another thread.
    pub fn claim(self: &Arc<Self>, tenant: &str) -> Result<BackupClaim, crate::error::ApiError> {
        let mut running = self
            .running
            .lock()
            .map_err(|_| crate::error::ApiError::Config("backup gate poisoned".to_string()))?;
        if running.contains(tenant) {
            return Err(crate::error::ApiError::BackupInProgress(tenant.to_string()));
        }
        running.insert(tenant.to_string());
        Ok(BackupClaim {
            tenant: tenant.to_string(),
            gate: Arc::clone(self),
        })
    }
}

/// RAII release of a tenant's backup slot. Dropping it (including on
/// unwind) frees the tenant for the next request.
#[derive(Debug)]
pub struct BackupClaim {
    tenant: String,
    gate: Arc<BackupGate>,
}

impl Drop for BackupClaim {
    fn drop(&mut self) {
        // Lock cannot be held across an await anywhere, so poisoning can
        // only come from a panic while holding it; ignore that (best
        // effort) and let the request layer surface the real error.
        if let Ok(mut running) = self.gate.running.lock() {
            running.remove(&self.tenant);
        }
    }
}

/// Outcome of a completed backup run.
#[derive(Debug, serde::Serialize)]
pub struct BackupOutcome {
    /// Exit status of the backup script.
    pub success: bool,
    /// Exit code, when the process terminated normally.
    pub code: Option<i32>,
    /// Combined stderr tail for operator debugging.
    pub stderr_tail: String,
    /// Wall-clock duration.
    pub duration_secs: u64,
}

/// Run `tenant-backup.sh <tenant> [--offsite]` under the gate.
///
/// # Errors
/// - [`crate::error::ApiError::BackupInProgress`] when a backup for this
///   tenant is already running.
/// - [`crate::error::ApiError::Config`] when the script is missing.
/// - [`crate::error::ApiError::Subprocess`] on spawn failure or timeout.
///   A *non-zero exit* of the script is a normal outcome
///   (`success: false`), not an error.
pub async fn run_backup(
    gate: &Arc<BackupGate>,
    script: &Path,
    tenant: &str,
    offsite: bool,
) -> Result<BackupOutcome, crate::error::ApiError> {
    let _claim = gate.claim(tenant)?;

    if !script.is_file() {
        return Err(crate::error::ApiError::Config(format!(
            "backup script {} not found",
            script.display()
        )));
    }

    let mut cmd = Command::new(script);
    cmd.arg(tenant);
    if offsite {
        cmd.arg("--offsite");
    }

    let started = std::time::Instant::now();
    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| crate::error::ApiError::Subprocess(format!("spawn failed: {e}")))?;

    match tokio::time::timeout(BACKUP_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let stderr_tail: String = String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .take(10)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            Ok(BackupOutcome {
                success: out.status.success(),
                code: out.status.code(),
                stderr_tail,
                duration_secs: started.elapsed().as_secs(),
            })
        }
        Ok(Err(e)) => Err(crate::error::ApiError::Subprocess(e.to_string())),
        Err(_) => Err(crate::error::ApiError::Subprocess(format!(
            "backup exceeded {}s timeout",
            BACKUP_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic)]
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gate_is_exclusive_per_tenant() {
        let gate = Arc::new(BackupGate::default());
        let _c1 = gate.claim("acme").expect("first claim");
        assert!(matches!(
            gate.claim("acme"),
            Err(crate::error::ApiError::BackupInProgress(_))
        ));
        // Other tenants unaffected.
        let _c2 = gate.claim("globex").expect("second tenant");
    }

    #[test]
    fn gate_releases_on_drop() {
        let gate = Arc::new(BackupGate::default());
        {
            let _c = gate.claim("acme").expect("claim");
        }
        assert!(gate.claim("acme").is_ok(), "slot should be free after drop");
    }

    #[tokio::test]
    async fn reports_missing_script() {
        let gate = Arc::new(BackupGate::default());
        let missing = PathBuf::from("/nonexistent/tenant-backup.sh");
        let err = run_backup(&gate, &missing, "acme", false)
            .await
            .expect_err("should fail");
        assert!(matches!(err, crate::error::ApiError::Config(_)));
    }

    #[tokio::test]
    async fn runs_a_trivial_script_and_captures_stderr() {
        let gate = Arc::new(BackupGate::default());
        let dir = std::env::temp_dir().join(format!("hosting-api-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let script = dir.join("fake-backup.sh");
        std::fs::write(&script, "#!/bin/sh\necho doing work >&2\nexit 0\n").expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        match run_backup(&gate, &script, "acme", false).await {
            Ok(outcome) => {
                assert!(outcome.success);
                assert!(outcome.stderr_tail.contains("doing work"));
            }
            Err(e) => panic!("run failed: {e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
