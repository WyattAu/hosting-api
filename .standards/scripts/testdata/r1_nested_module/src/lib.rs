//! A `pub enum` nested inside a public module, with a feature-gated variant.
//! Module nesting must not hide the variant from R1 — `pub mod` re-exports the
//! enum just as effectively as a top-level `pub`. This is the same shape as
//! `worker-kit`'s `Trigger::Cron` and `metrics-kit`'s `Format::OpenMetrics`.
//!
//! The gated arm in `render` must be reported as R1 only: because the enum is
//! crate-local, R3 must stay silent rather than double-reporting one gate.

/// Rendering backend selection.
pub mod backend {
    /// Which exposition backend to use.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Backend {
        /// Prometheus text format.
        PromText,
        /// OpenMetrics text format.
        #[cfg(feature = "openmetrics")]
        OpenMetrics,
    }
}

/// Render `family` in the requested backend.
pub fn render(family: &str, backend: backend::Backend) -> String {
    match backend {
        backend::Backend::PromText => format!("# TYPE {family} counter\n"),
        #[cfg(feature = "openmetrics")]
        backend::Backend::OpenMetrics => format!("# TYPE {family} counter\n# EOF\n"),
    }
}
