//! Durable metering journal (SIS research loop round 6, iter 51).
//!
//! The scraper ([`crate::usage`]) is deliberately in-memory: scrapes are
//! cheap, lossy-by-design, and a restart only loses the *current partial*
//! window. What must never be lost is a CLOSED billing window, so at each
//! window boundary the charge is posted to a double-entry ledger
//! ([`ledger_kit`]) whose journal mirrors every accepted posting into an
//! outbox-kit store — durable across restarts, replayable, idempotent on
//! the posting id.
//!
//! Account model (one currency, the metering config's):
//!
//! ```text
//! tenant:<slug>   [debit]  -- the tenant's accumulated charges
//! platform:metering-revenue [credit] -- the platform's mirror account
//! ```
//!
//! Balances are DERIVED by ledger-kit (a pure fold over postings), so a
//! tenant's statement is reproducible from the journal alone. The
//! idempotency key is `metering:{tenant}:{start}:{end}` — the same closed
//! window cannot post twice, even across a crash between scrape and post.
//!
//! Durability tiering: with `HOSTING_LEDGER_DB` set the outbox is
//! outbox-kit's `SQLite` store (single file, rusqlite bundled); without it
//! the ledger is in-memory (zero new dependencies at runtime, matching
//! the current no-DB deployments). With `HOSTING_EVENTBUS_URL` +
//! `HOSTING_EVENTBUS_TOKEN` also set, an outbox-kit dispatcher delivers
//! every posting to eventbusd (`hosting.billing.window`) with
//! at-least-once semantics, breaker-aware backoff, and parked retries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ledger_kit::{Account, AccountBalance, AccountId, Journal, Ledger, LedgerError, OutboxJournal};
use outbox_kit::{MemoryStore, OutboxStore};
use rust_decimal::Decimal;

use crate::usage::{MeteringConfig, TenantUsage, UsageError};

/// The platform-side mirror account every metering posting credits.
pub const PLATFORM_ACCOUNT: &str = "platform.metering-revenue";
/// Account-id prefix for tenant charge accounts: `tenant.<slug>`.
pub const TENANT_ACCOUNT_PREFIX: &str = "tenant.";

/// Ledger errors, re-mapped into the metering error universe.
#[derive(Debug, thiserror::Error)]
pub enum LedgerUsageError {
    /// The ledger rejected a posting or account.
    #[error("ledger: {0}")]
    Ledger(String),
}

impl From<LedgerError> for UsageError {
    fn from(e: LedgerError) -> Self {
        UsageError::Billing(format!("ledger: {e}"))
    }
}

/// Tenant account id for a slug: `tenant.<slug>` (ledger-kit account ids
/// allow `[a-z0-9_.-]`; slugs are already lowercase `[a-z0-9-]`).
fn tenant_account(slug: &str) -> Result<AccountId, UsageError> {
    AccountId::new(format!("{TENANT_ACCOUNT_PREFIX}{slug}"))
        .map_err(|e| UsageError::Billing(format!("tenant account id: {e}")))
}

/// The durable metering journal.
pub struct UsageLedger {
    ledger: Ledger<OutboxJournal>,
    platform: AccountId,
    /// The `SQLite` path when durable, `None` for in-memory.
    db_path: Option<PathBuf>,
}

impl UsageLedger {
    /// Open the ledger: SQLite-backed outbox when `db_path` is set,
    /// in-memory otherwise. Opens (or reuses) the platform account.
    ///
    /// # Errors
    /// Returns [`UsageError`] for an unusable database path or an
    /// invalid platform account id.
    pub fn open(db_path: Option<&Path>, cfg: &MeteringConfig) -> Result<Self, UsageError> {
        let outbox: Arc<dyn OutboxStore> = match db_path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        UsageError::Billing(format!("ledger db dir {}: {e}", parent.display()))
                    })?;
                }
                Arc::new(outbox_kit::SqliteStore::open(path).map_err(|e| {
                    UsageError::Billing(format!("ledger db {}: {e}", path.display()))
                })?)
            }
            None => Arc::new(MemoryStore::new()),
        };
        let journal = OutboxJournal::with_inner_and_store(ledger_kit::MemoryJournal::new(), outbox);
        let ledger = Ledger::with_journal(journal);
        let platform = AccountId::new(PLATFORM_ACCOUNT)?;
        if ledger.account(&platform).is_none() {
            ledger.open_account(platform.clone(), cfg.currency)?;
        }
        Ok(Self {
            ledger,
            platform,
            db_path: db_path.map(Path::to_path_buf),
        })
    }

    /// True when the outbox is a durable `SQLite` store (statements and
    /// idempotency survive restarts).
    #[must_use]
    pub fn durable(&self) -> bool {
        self.db_path.is_some()
    }

    /// Replay undelivered outbox envelopes into the fast journal. Call
    /// once at startup after [`UsageLedger::open`] — at-least-once,
    /// idempotent on the posting id on both sides.
    ///
    /// # Errors
    /// Returns [`UsageError`] if the outbox backend fails.
    pub async fn restore(&self) -> Result<usize, UsageError> {
        self.ledger
            .journal()
            .restore()
            .await
            .map_err(UsageError::from)
    }

    /// The platform account (useful for statements and tests).
    ///
    /// # Errors
    /// Returns [`UsageError`] if the platform account is somehow absent.
    pub fn platform_account(&self) -> Result<Account, UsageError> {
        self.ledger
            .account(&self.platform)
            .ok_or_else(|| UsageError::Billing("platform account missing".to_string()))
    }

    /// Post one closed metering window. Idempotent across restarts: the
    /// memo carries a machine-matchable idempotency token
    /// (`idem=metering:{tenant}:{start}:{end}`) and the tenant's journalled
    /// postings are checked for it BEFORE posting — the journal itself is
    /// the durable claim, so a double close (crash after post, replay,
    /// operator rerun) resolves to the existing fact. Zero-value windows
    /// are deliberately NOT posted: a tenant that used nothing owes
    /// nothing, and the ledger should not grow noise.
    ///
    /// Invariant: [`UsageLedger::restore`] must have run since process
    /// start (main spawns it at boot) so replayed postings are visible to
    /// the pre-check.
    ///
    /// Returns the accepted posting, or `None` when the window was already
    /// journalled or carried zero charge.
    ///
    /// # Errors
    /// Returns [`UsageError`] for pricing or ledger failures.
    pub async fn record_window(
        &self,
        usage: &TenantUsage,
        cfg: &MeteringConfig,
    ) -> Result<Option<ledger_kit::Posting>, UsageError> {
        let charge = usage.charge(cfg)?;
        if charge.amount.is_zero() {
            return Ok(None);
        }
        let tenant = tenant_account(&usage.tenant)?;
        let token = format!(
            "idem=metering:{}:{}:{}",
            usage.tenant,
            usage.window_start.timestamp(),
            usage.window_end.timestamp()
        );
        // Durable claim check: the journal is the source of truth, and a
        // replayed posting is as real as one posted in this process.
        for existing in self
            .ledger
            .journal()
            .postings_for(&tenant, usize::MAX)
            .await?
        {
            if existing.memo().contains(&token) {
                return Ok(None);
            }
        }
        if self.ledger.account(&tenant).is_none() {
            self.ledger.open_account(tenant.clone(), cfg.currency)?;
        }
        let memo = format!(
            "metering window {}..{} (cpu {}s, mem {}GiB*s) [{token}]",
            usage.window_start.to_rfc3339(),
            usage.window_end.to_rfc3339(),
            usage.cpu_core_seconds,
            usage.memory_gibibyte_seconds
        );
        match self
            .ledger
            .post(&tenant, &self.platform, charge, &memo, &token)
            .await
        {
            Ok(posting) => Ok(Some(posting)),
            // Same-process double post raced past the pre-check; the
            // ledger's claim store caught it — the fact stands.
            Err(LedgerError::DuplicatePosting { .. }) => Ok(None),
            Err(e) => Err(UsageError::from(e)),
        }
    }

    /// Derived tenant statement: the account balance plus the most
    /// recent `limit` postings touching the tenant account. A real
    /// tenant that has never been billed yields a zero balance and no
    /// postings (only unknown slugs error, via [`Self::tenant_account`]).
    ///
    /// # Errors
    /// Returns [`UsageError`] for store failures.
    pub async fn tenant_statement(
        &self,
        slug: &str,
        limit: usize,
        currency: ledger_kit::Currency,
    ) -> Result<(AccountBalance, Vec<ledger_kit::Posting>), UsageError> {
        let account = tenant_account(slug)?;
        let registered = match self.ledger.account(&account) {
            Some(a) => a,
            None => Account::new(account.as_str().to_owned(), currency)
                .map_err(|e| UsageError::Billing(format!("tenant account: {e}")))?,
        };
        let postings = self.ledger.journal().postings_for(&account, limit).await?;
        let balance = AccountBalance::derive(registered, &postings)?;
        Ok((balance, postings))
    }

    /// Total platform-side revenue posted, derived from the journal
    /// (credit side of the platform account).
    ///
    /// # Errors
    /// Returns [`UsageError`] if the store fails.
    pub async fn platform_total(&self, _cfg: &MeteringConfig) -> Result<Decimal, UsageError> {
        let postings = self.ledger.journal().all_postings(usize::MAX).await?;
        let total = postings
            .iter()
            .filter(|p| p.credit() == &self.platform)
            .fold(Decimal::ZERO, |acc, p| acc + p.amount().amount);
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use chrono::{DateTime, Utc};
    use rust_decimal_macros::dec;

    fn at(minute: u32) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + i64::from(minute) * 60, 0).unwrap_or_default()
    }

    fn cfg() -> MeteringConfig {
        MeteringConfig {
            currency: billing_kit::Currency::GBP,
            cpu_price_per_core_hour: dec!(0.01),
            memory_price_per_gib_hour: Decimal::ZERO,
            discount_percent: Decimal::ZERO,
        }
    }

    fn usage(tenant: &str, cpu: Decimal, start: u32, end: u32) -> TenantUsage {
        TenantUsage {
            tenant: tenant.to_string(),
            window_start: at(start),
            window_end: at(end),
            cpu_core_seconds: cpu,
            memory_gibibyte_seconds: Decimal::ZERO,
        }
    }

    #[tokio::test]
    async fn posts_once_for_a_window_and_derives_the_balance() {
        let ledger = UsageLedger::open(None, &cfg()).expect("open");
        let u = usage("acme", dec!(3600), 0, 60); // 1 core-hour = 0.01
        let first = ledger.record_window(&u, &cfg()).await.expect("post");
        assert!(first.is_some(), "first post accepts");
        let second = ledger
            .record_window(&u, &cfg())
            .await
            .expect("duplicate resolves");
        assert!(second.is_none(), "duplicate window is a no-op");
        let (balance, postings) = ledger
            .tenant_statement("acme", 10, cfg().currency)
            .await
            .expect("statement");
        assert_eq!(postings.len(), 1, "exactly one posting");
        assert_eq!(
            balance.balance.amount,
            dec!(-0.01),
            "tenant is debited: derived balance is negative owed"
        );
        assert_eq!(
            ledger.platform_total(&cfg()).await.expect("total"),
            dec!(0.01)
        );
    }

    #[tokio::test]
    async fn zero_windows_are_not_posted() {
        let ledger = UsageLedger::open(None, &cfg()).expect("open");
        let posted = ledger
            .record_window(&usage("acme", Decimal::ZERO, 0, 60), &cfg())
            .await
            .expect("zero window");
        assert!(posted.is_none());
        let (balance, postings) = ledger
            .tenant_statement("acme", 10, cfg().currency)
            .await
            .expect("zero-history tenant still gets a statement");
        assert!(postings.is_empty());
        assert_eq!(balance.balance.amount, Decimal::ZERO);
    }

    #[tokio::test]
    async fn sqlite_outbox_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let db = dir.path().join("ledger.db");
        let u = usage("acme", dec!(7200), 0, 60); // 2 core-hours = 0.02
        {
            let ledger = UsageLedger::open(Some(&db), &cfg()).expect("open");
            ledger.record_window(&u, &cfg()).await.expect("post");
        }
        // Reopen: reads come from the fast journal, which starts EMPTY —
        // restore() replays the durable outbox into it (at-least-once).
        let reopened = UsageLedger::open(Some(&db), &cfg()).expect("reopen");
        let restored = reopened.restore().await.expect("restore");
        assert_eq!(restored, 1, "the posting replays from the outbox");
        assert_eq!(
            reopened.platform_total(&cfg()).await.expect("total"),
            dec!(0.02),
            "posting survives the restart"
        );
        let again = reopened
            .record_window(&u, &cfg())
            .await
            .expect("re-post after restart");
        assert!(
            again.is_none(),
            "same window stays idempotent across restarts"
        );
    }
}
