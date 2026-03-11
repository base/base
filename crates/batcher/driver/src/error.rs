use base_comp::ChannelOutError;

/// Errors returned by [`ChannelDriver`](crate::ChannelDriver).
#[derive(Debug, thiserror::Error)]
pub enum ChannelDriverError {
    /// No batches have been added; there is nothing to flush.
    #[error("no batches queued")]
    Empty,
    /// An error from the underlying [`ChannelOut`](base_comp::ChannelOut).
    #[error("channel error: {0}")]
    Channel(#[from] ChannelOutError),
}
