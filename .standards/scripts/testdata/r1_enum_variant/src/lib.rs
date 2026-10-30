//! Reproduces the outbox-kit 0.1.0 regression shape: `breaker`'s additive
//! `timeout` feature appends a variant to a public enum, and any host that
//! enables it graph-wide breaks every exhaustive match.

/// Why a dispatch failed.
#[derive(Debug)]
pub enum DispatchError {
    /// The store rejected the write.
    Store(String),
    /// The breaker shed the call.
    Rejected,
    /// Feature-gated variant — the unsound element.
    #[cfg(feature = "timeout")]
    Timeout,
}
