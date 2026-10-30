//! Same shape as the outbox-kit regression, but the enum is private and never
//! appears in the public API. Gating a private enum variant is sound: no
//! downstream crate can match on it. The checker must stay silent.

#[derive(Debug)]
enum InternalState {
    Idle,
    Busy,
    #[cfg(feature = "timeout")]
    TimedOut,
}

impl InternalState {
    /// Read the state, folding the optional variant into a stable answer.
    pub fn is_stuck(&self) -> bool {
        match self {
            Self::Idle => false,
            Self::Busy => false,
            #[cfg(feature = "timeout")]
            Self::TimedOut => true,
        }
    }
}
