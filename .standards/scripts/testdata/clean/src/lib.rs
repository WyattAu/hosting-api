//! Real-world shapes that must stay silent, harvested from estate crates after
//! the first sweep produced 32 findings of which 26 were noise. Each case here
//! corresponds to a bug this checker actually had.

use std::collections::BTreeMap;

/// A private field on a public struct. Hosts cannot build the struct with a
/// literal once *any* field is private, so gating a private field is invisible
/// downstream. Found in `book-kit` (`Book::shared`) and `fetch-kit`
/// (`ClientBuilder::retries`).
#[derive(Debug)]
pub struct Book {
    /// Present only when the arena is heap-allocated.
    #[cfg(feature = "alloc")]
    shared: std::sync::Arc<Shared>,
    depth: u32,
}

/// `pub(crate)` is not public API. `TelemetryConfig` in `telemetry-init` is the
/// shape: restricted fields gated per feature, invisible to every host.
#[derive(Debug)]
pub struct TelemetryConfig {
    /// Only recorded when metrics are on.
    #[cfg(feature = "metrics")]
    pub(crate) metrics_budget: usize,
    pub(crate) service_name: String,
}

/// A `#[non_exhaustive]` struct may gain and lose fields freely. Calibre-style
/// config structs in the estate already use this escape hatch.
#[derive(Debug)]
#[non_exhaustive]
pub struct OpenOptions {
    /// Only present with the timeout feature.
    #[cfg(feature = "timeout")]
    pub timeout: std::time::Duration,
    pub retries: u32,
}

/// Statements inside a match body are not arms. Gating them is sound and is
/// extremely common: `retry-backoff` gates `tracing::warn!`/`error!`/`debug!`
/// calls inside a match, `metrics-kit` gates an `if let` inside a match arm.
pub fn classify(n: i32) -> &'static str {
    match n {
        0 => "zero",
        #[cfg(feature = "tracing")]
        {
            tracing::warn!("non-zero input: {n}");
            "nonzero"
        }
        other => {
            #[cfg(feature = "tracing")]
            if other > 100 {
                tracing::warn!("large input: {other}");
            }
            "other"
        }
    }
}

/// A gated arm over an enum this crate owns is self-consistent even when the
/// enum is declared in a *sibling module* — ownership is crate-wide, not
/// file-wide, and a private enum has no public API for a host to observe.
pub mod inner {
    /// Crate-private, so gating its variant is invisible downstream.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Mode {
        /// Fast path.
        Fast,
        /// Only with the slow feature.
        #[cfg(feature = "slow")]
        Slow,
    }
}

pub fn render(mode: inner::Mode) -> &'static str {
    match mode {
        inner::Mode::Fast => "fast",
        #[cfg(feature = "slow")]
        inner::Mode::Slow => "slow",
    }
}

/// Gating a whole module, type, function, or impl block adds capability
/// without reshaping anything that already exists.
#[cfg(feature = "tracing")]
pub mod telemetry {
    /// Emit one span.
    pub fn span() {}
}

#[cfg(feature = "pool")]
#[derive(Debug, Default)]
pub struct Pool {
    /// Idle connection ceiling.
    pub max_idle: usize,
}

#[cfg(feature = "pool")]
impl Pool {
    /// Whether pooling is on.
    pub fn enabled(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_zero() {
        assert_eq!(classify(0), "zero");
    }

    #[cfg(feature = "slow")]
    #[test]
    fn slow_mode_renders() {
        assert_eq!(render(inner::Mode::Slow), "slow");
    }
}

// Keep the shared type referenced so the fixture compiles standalone.
#[derive(Debug)]
pub struct Shared;

/// Unused import guard for the BTreeMap above.
pub type Registry = BTreeMap<String, u32>;
