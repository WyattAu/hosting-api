//! A match arm gated behind a feature. When the scrutinee enum has
//! feature-gated variants, this arm silently disappears under unification and
//! an exhaustive match stops being exhaustive. This is the shape the estate
//! fixed by replacing the gated arm with an unconditional catch-all.

use breaker::{CircuitBreakerError, DispatchError};

/// Classify a breaker outcome for the metrics registry.
pub fn classify(err: &CircuitBreakerError) -> DispatchError {
    match err {
        CircuitBreakerError::CircuitOpen => DispatchError::Rejected,
        CircuitBreakerError::Failure(_) => DispatchError::Store("send failed".into()),
        // Only compiled when this crate's own `timeout` feature is on, which
        // is not the same question as whether the *host* enabled
        // `breaker/timeout`.
        #[cfg(feature = "timeout")]
        CircuitBreakerError::Timeout => DispatchError::Store("timed out".into()),
    }
}
