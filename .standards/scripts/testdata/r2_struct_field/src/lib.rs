//! A public struct whose field exists only under a feature. Hosts that
//! construct or destructure the struct with literals break as soon as the
//! feature is unified on, and hosts compiled without it see a different type.

/// Connection tuning knobs.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum idle connections per host.
    pub max_idle: usize,
    /// Whether to keep connections warm between requests.
    pub keep_alive: bool,
    /// Only present when the pool feature is on.
    #[cfg(feature = "pool")]
    pub idle_timeout: std::time::Duration,
}
