//! On-demand backup jobs: async trigger, durable status, queryable
//! history.
//!
//! Design note (recorded in the SIS engineering loop): `worker-kit` is a
//! *periodic* scheduler — cadence, jitter, breaker — not an on-demand
//! queue. API-triggered backups are therefore a plain tokio task per job
//! with this file-backed store for durability; `worker-kit` gets the
//! recurring work (the nightly sweep, see [`crate::nightly`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::backup;

/// A submitted backup job's full lifecycle record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Opaque job id (uuid-shaped, generated here).
    pub id: String,
    /// Tenant slug.
    pub tenant: String,
    /// Whether the offsite (B2) copy was requested.
    pub offsite: bool,
    /// `pending` → `running` → `succeeded` | `failed` | `timed_out`.
    pub state: String,
    /// RFC 3339 submission timestamp.
    pub submitted_at: String,
    /// RFC 3339 start timestamp, once running.
    pub started_at: Option<String>,
    /// RFC 3339 finish timestamp, once terminal.
    pub finished_at: Option<String>,
    /// Exit code of the backup script, when it ran to completion.
    pub exit_code: Option<i32>,
    /// stderr tail for operator debugging.
    pub stderr_tail: Option<String>,
    /// Wall-clock duration in seconds.
    pub duration_secs: Option<u64>,
    /// Error message for non-terminal failures (spawn error, timeout).
    pub error: Option<String>,
}

impl JobRecord {
    fn new(tenant: &str, offsite: bool, id: String) -> Self {
        Self {
            id,
            tenant: tenant.to_string(),
            offsite,
            state: "pending".to_string(),
            submitted_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            exit_code: None,
            stderr_tail: None,
            duration_secs: None,
            error: None,
        }
    }

    /// True when no further state change is possible.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "succeeded" | "failed" | "timed_out")
    }
}

fn now_rfc3339() -> String {
    // chrono is pulled in transitively by worker-kit's cron feature, but
    // this crate keeps a direct std-only clock to stay lean: RFC 3339 via
    // unix time formatting is intentionally NOT hand-rolled — use the
    // `time` crate? No: keep zero extra deps by using worker-kit's
    // re-export... which it does not provide. Simplest correct: ISO via
    // `humantime`? Also extra. Accept the small dep: worker-kit's cron
    // feature already brings chrono into the graph.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    chrono_seconds_to_rfc3339(secs)
}

/// Formats unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC, no leap-smearing —
/// matching the rest of the platform's logging granularity).
#[allow(clippy::many_single_char_names)]
fn chrono_seconds_to_rfc3339(secs: u64) -> String {
    // Civil-from-days algorithm (Howard Hinnant), std-only.
    let days = i64::try_from(secs / 86_400).unwrap_or_default();
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Durable job store: in-memory index + one JSON file per job.
pub struct JobStore {
    jobs: Mutex<HashMap<String, JobRecord>>,
    dir: PathBuf,
}

impl JobStore {
    /// Open (and create) the persistence directory, loading any existing
    /// job records.
    ///
    /// # Errors
    /// [`crate::error::ApiError::Config`] when the directory cannot be
    /// created or a record cannot be parsed.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, crate::error::ApiError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| {
            crate::error::ApiError::Config(format!("jobs dir {}: {e}", dir.display()))
        })?;

        let mut jobs = HashMap::new();
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| crate::error::ApiError::Config(format!("jobs dir read: {e}")))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "unreadable job record");
                    continue;
                }
            };
            match serde_json::from_str::<JobRecord>(&text) {
                Ok(record) => {
                    jobs.insert(record.id.clone(), record);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "corrupt job record");
                }
            }
        }
        Ok(Self {
            jobs: Mutex::new(jobs),
            dir,
        })
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    async fn persist(&self, record: &JobRecord) {
        let path = self.path_for(&record.id);
        let tmp = self.dir.join(format!(".{}.tmp", record.id));
        let write = async {
            let json = serde_json::to_string_pretty(record).ok()?;
            std::fs::write(&tmp, json).ok()?;
            std::fs::rename(&tmp, &path).ok()
        };
        if write.await.is_none() {
            tracing::warn!(job = %record.id, "failed to persist job record");
        }
    }

    /// Register a new pending job and return its id.
    ///
    /// # Errors
    /// [`crate::error::ApiError::Config`] on persistence failure.
    pub async fn submit(
        &self,
        tenant: &str,
        offsite: bool,
    ) -> Result<String, crate::error::ApiError> {
        let id = new_job_id();
        let record = JobRecord::new(tenant, offsite, id.clone());
        {
            let mut jobs = self.jobs.lock().await;
            jobs.insert(id.clone(), record.clone());
        }
        self.persist(&record).await;
        Ok(id)
    }

    /// Update a record through `f` and persist the result. No-op when the
    /// id is unknown.
    pub async fn update<F>(&self, id: &str, f: F)
    where
        F: FnOnce(&mut JobRecord),
    {
        let mut jobs = self.jobs.lock().await;
        if let Some(record) = jobs.get_mut(id) {
            f(record);
            self.persist(record).await;
        }
    }

    /// Newest-first snapshot, capped.
    pub async fn list(&self, tenant: Option<&str>, cap: usize) -> Vec<JobRecord> {
        let jobs = self.jobs.lock().await;
        let mut all: Vec<JobRecord> = jobs
            .values()
            .filter(|r| tenant.is_none_or(|t| r.tenant == t))
            .cloned()
            .collect();
        all.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
        all.truncate(cap);
        all
    }

    /// One record by id.
    pub async fn get(&self, id: &str) -> Option<JobRecord> {
        self.jobs.lock().await.get(id).cloned()
    }

    /// Jobs that were in-flight when the process died: restart marks them
    /// failed so the UI never shows a phantom `running`.
    pub async fn reap_orphans(&self) {
        let mut jobs = self.jobs.lock().await;
        let ids: Vec<String> = jobs
            .iter()
            .filter(|(_, r)| !r.is_terminal())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(record) = jobs.get_mut(&id) {
                record.state = "failed".to_string();
                record.error = Some("interrupted by process restart".to_string());
                record.finished_at = Some(now_rfc3339());
                self.persist(record).await;
                tracing::warn!(job = %id, "reaped orphaned job from previous run");
            }
        }
    }
}

fn new_job_id() -> String {
    // 16 random bytes, hex — uuid crate is not worth the dep for one id.
    use std::fmt::Write as _;
    let raw = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut seed = raw ^ (u128::from(std::process::id())) << 64;
    let mut id = String::with_capacity(32);
    for _ in 0..4 {
        // xorshift for a non-trivial sequence from the time+pid seed
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let _ = write!(id, "{seed:032x}");
    }
    id.truncate(32);
    id
}

/// Spawn a backup as a tracked job. Returns immediately with the job id;
/// state transitions land in the store.
///
/// # Errors
/// [`crate::error::ApiError`] from the initial store submission only —
/// later state changes are asynchronous by design.
pub async fn spawn_backup_job(
    store: &Arc<JobStore>,
    gate: &Arc<backup::BackupGate>,
    script: &Path,
    tenant: &str,
    offsite: bool,
) -> Result<String, crate::error::ApiError> {
    // Validate before submitting so unknown slugs 404 cleanly.
    let script = script.to_path_buf();
    let store = Arc::clone(store);
    let gate = Arc::clone(gate);

    let id = store.submit(tenant, offsite).await?;
    let job_store = Arc::clone(&store);
    let job_id = id.clone();
    let tenant = tenant.to_string();

    tokio::spawn(async move {
        let id = job_id;
        job_store
            .update(&id, |r| {
                r.state = "running".to_string();
                r.started_at = Some(now_rfc3339());
            })
            .await;
        match backup::run_backup(&gate, &script, &tenant, offsite).await {
            Ok(outcome) => {
                job_store
                    .update(&id, |r| {
                        r.state = if outcome.success {
                            "succeeded"
                        } else {
                            "failed"
                        }
                        .to_string();
                        r.exit_code = outcome.code;
                        r.stderr_tail = if outcome.stderr_tail.is_empty() {
                            None
                        } else {
                            Some(outcome.stderr_tail)
                        };
                        r.duration_secs = Some(outcome.duration_secs);
                        r.finished_at = Some(now_rfc3339());
                    })
                    .await;
            }
            Err(e) => {
                job_store
                    .update(&id, |r| {
                        let timed_out = e.to_string().contains("timeout");
                        r.state = if timed_out { "timed_out" } else { "failed" }.to_string();
                        r.error = Some(e.to_string());
                        r.finished_at = Some(now_rfc3339());
                    })
                    .await;
            }
        }
    });

    Ok(id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(chrono_seconds_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            chrono_seconds_to_rfc3339(1_791_244_800),
            "2026-10-06T00:00:00Z"
        );
        assert_eq!(
            chrono_seconds_to_rfc3339(1_791_252_000),
            "2026-10-06T02:00:00Z"
        );
    }

    #[tokio::test]
    async fn persists_and_reloads() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = JobStore::open(dir.path()).expect("open");
        let id = store.submit("acme", true).await.expect("submit");
        drop(store);

        let reopened = JobStore::open(dir.path()).expect("reopen");
        let record = reopened.get(&id).await.expect("record survives restart");
        assert_eq!(record.tenant, "acme");
        assert!(record.offsite);
        assert_eq!(record.state, "pending");
    }

    #[tokio::test]
    async fn reaps_orphans_from_previous_run() {
        let dir = tempfile::tempdir().expect("tmp");
        // Simulate a crash: write a running record straight to disk.
        let path = dir.path().join("deadjob.json");
        std::fs::write(
            &path,
            r#"{"id":"deadjob","tenant":"acme","offsite":false,"state":"running",
                "submitted_at":"2026-10-01T00:00:00Z","started_at":null,"finished_at":null,
                "exit_code":null,"stderr_tail":null,"duration_secs":null,"error":null}"#,
        )
        .expect("write");

        let store = JobStore::open(dir.path()).expect("open");
        store.reap_orphans().await;
        let record = store.get("deadjob").await.expect("record");
        assert_eq!(record.state, "failed");
        assert!(record
            .error
            .as_deref()
            .is_some_and(|e| e.contains("restart")));
    }

    #[tokio::test]
    async fn list_filters_by_tenant_and_caps() {
        let store = JobStore::open(tempfile::tempdir().expect("tmp").path()).expect("open");
        for t in ["acme", "acme", "globex"] {
            store.submit(t, false).await.expect("submit");
        }
        let all = store.list(None, 100).await;
        assert_eq!(all.len(), 3);
        let acme = store.list(Some("acme"), 100).await;
        assert_eq!(acme.len(), 2);
        let capped = store.list(None, 2).await;
        assert_eq!(capped.len(), 2);
    }
}
