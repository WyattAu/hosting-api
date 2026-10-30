//! A required method of a public trait gated behind a feature. Every host
//! must provide an impl, but a host that did not ask for the feature cannot
//! know the method exists — so enabling the feature anywhere in the graph
//! breaks hosts that were previously complete.

/// A sink that accepts audit records.
pub trait AuditSink {
    /// Persist one record. Always required.
    fn write(&mut self, line: &str);

    /// Flush buffered records. Only required with the `fsync` feature.
    #[cfg(feature = "fsync")]
    fn sync(&mut self) -> std::io::Result<()>;
}
