//! A vendored dependency's source is not the estate's code. The rules must not
//! fire on it.
//!
//! Real case: `clawdius/.cargo-vendor/half/src/slice.rs` was reported by an
//! earlier revision of the checker, which globbed `*/src` without excluding
//! vendor directories. Both the candidate root and any nested path may sit
//! under a dot-directory or a known vendor name.

/// Half-precision float storage.
#[derive(Debug)]
pub enum Half {
    /// A stored value.
    Bits(u16),
    /// Only with the alloc feature.
    #[cfg(feature = "alloc")]
    Owned(Vec<u16>),
}
