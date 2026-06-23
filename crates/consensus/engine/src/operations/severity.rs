//! Error severity classification for direct engine operations.

/// The severity of an engine operation error.
#[derive(Debug, PartialEq, Eq, derive_more::Display, Clone, Copy)]
pub enum EngineTaskErrorSeverity {
    /// The error is temporary and the operation is retried.
    #[display("temporary")]
    Temporary,
    /// The error is critical and is propagated to the engine actor.
    #[display("critical")]
    Critical,
    /// The error indicates that the engine should be reset.
    #[display("reset")]
    Reset,
    /// The error indicates that the derivation pipeline should be flushed.
    #[display("flush")]
    Flush,
}

/// Error classification for direct engine operations.
pub trait EngineTaskError {
    /// The severity of the error.
    fn severity(&self) -> EngineTaskErrorSeverity;
}

impl EngineTaskErrorSeverity {
    /// Returns a static string label for use in metrics.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Temporary => "temporary",
            Self::Critical => "critical",
            Self::Reset => "reset",
            Self::Flush => "flush",
        }
    }
}
