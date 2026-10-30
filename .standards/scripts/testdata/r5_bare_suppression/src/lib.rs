//! A suppression with no reason. The estate does not accept undocumented
//! escape hatches, so an empty reason is itself a finding (R5) — otherwise
//! `allow` becomes a silent blanket suppression.

/// Why a read failed.
#[derive(Debug)]
pub enum ReadError {
    /// The underlying device returned an error.
    Io(String),
    // feature-compat: allow
    #[cfg(feature = "mmap")]
    Truncated,
}
