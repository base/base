use thiserror::Error;

/// Errors that can occur during witness transport operations.
#[derive(Error, Debug)]
pub enum TransportError {
    /// The underlying channel was closed by the remote end.
    #[error("channel closed")]
    ChannelClosed,

    /// Sending a message through the channel failed.
    #[error("send failed: {0}")]
    Send(String),
}
