use std::sync::mpsc::SendError;

use alloy_eips::BlockId;
use thiserror::Error;

use crate::DerivationClientError;

/// The error type for the `L1WatcherActor`.
#[derive(Error, Debug)]
pub enum L1WatcherActorError<T> {
    /// Error sending the head update event.
    #[error("Error sending the head update event: {0}")]
    SendError(#[from] SendError<T>),
    /// Error from the L1 block fetcher.
    #[error("L1 block fetcher error: {0}")]
    Fetcher(String),
    /// The L1 block was not found.
    #[error("L1 block not found: {0}")]
    L1BlockNotFound(BlockId),
    /// Stream ended unexpectedly.
    #[error("Stream ended unexpectedly")]
    StreamEnded,
    /// Derivation client error.
    #[error("derivation client error: {0}")]
    DerivationClientError(#[from] DerivationClientError),
}
