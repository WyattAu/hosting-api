//! A gated field on a `#[non_exhaustive]` struct is the documented escape
//! hatch: downstream crates cannot construct the struct with a literal, so
//! adding or removing a field is not a breaking change. The checker must stay
//! silent.

/// Retry tuning knobs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RetryConfig {
    /// Base backoff interval.
    pub base: std::time::Duration,
    /// Only present when the jitter feature is on.
    #[cfg(feature = "jitter")]
    pub max_jitter: std::time::Duration,
}
