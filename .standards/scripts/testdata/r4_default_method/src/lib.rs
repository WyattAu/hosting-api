//! A trait method that is gated but *has a default body*. Hosts that never
//! enable the feature still compile against the default, so unification is
//! harmless. This is the documented way to add an optional trait capability,
//! and the checker must stay silent.

/// A clock a job can read.
pub trait Clock {
    /// Nanoseconds since an arbitrary epoch.
    fn now_nanos(&self) -> u64;

    /// High-resolution reading, when the feature is on.
    #[cfg(feature = "tsc")]
    fn now_tsc(&self) -> u64 {
        self.now_nanos()
    }
}
