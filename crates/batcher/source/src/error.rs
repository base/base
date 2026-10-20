//! Error type of the L2 block and L1 head polling adapters.

/// Errors produced by [`PollingSource`][crate::PollingSource] and
/// [`L1HeadPolling`][crate::L1HeadPolling]. The sources built on them retry.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Provider or RPC error.
    #[error("provider error: {0}")]
    Provider(String),
    /// A requested block has not become available from the provider yet.
    #[error("block {0} is not available yet")]
    BlockUnavailable(u64),
}
