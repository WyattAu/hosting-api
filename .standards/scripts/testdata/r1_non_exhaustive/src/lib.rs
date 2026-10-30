//! A `#[non_exhaustive]` enum with a feature-gated variant — the sanctioned
//! remedy, and therefore silent.
//!
//! `#[non_exhaustive]` converts "an unrelated crate in my graph silently breaks
//! every exhaustive match" into "one compile error at the point of use, with a
//! suggested `_` arm". That is a better failure than the gate can be, so R1
//! stands down.
//!
//! This is the shape `worker-kit` uses for `Trigger` (`Trigger::Cron` under the
//! `cron` feature) and `RegisterError` (`RegisterError::InvalidCron`).
//!
//! Note the attribute is separated from the item by a `derive` — the exact
//! layout an earlier revision of the checker failed to associate, reporting R1
//! on a type that had opted out.

/// What starts a job.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Trigger {
    /// Fire every `interval`.
    Interval(std::time::Duration),
    /// Only with the `cron` feature.
    #[cfg(feature = "cron")]
    Cron(String),
}

/// A gated field on a `#[non_exhaustive]` struct is likewise sanctioned: hosts
/// cannot build the struct with a literal, so no host can observe the field
/// appearing or disappearing.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RetryConfig {
    /// Base backoff.
    pub base: std::time::Duration,
    /// Only present with the `jitter` feature.
    #[cfg(feature = "jitter")]
    pub max_jitter: std::time::Duration,
}

/// Matching inside the crate still sees every variant, so no internal arm is
/// lost.
pub fn is_cron(t: &Trigger) -> bool {
    match t {
        Trigger::Interval(_) => false,
        #[cfg(feature = "cron")]
        Trigger::Cron(_) => true,
    }
}