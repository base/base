//! Throttle client trait for applying DA limits to the block builder.

use std::sync::Arc;

use futures::future::BoxFuture;

/// Applies throttle parameters to a block-builder endpoint.
///
/// The canonical implementation calls the `miner_setMaxDASize` RPC method
/// on the L2 execution client, which instructs the sequencer to limit the
/// amount of DA-eligible data it accepts per transaction and per block.
pub trait ThrottleClient: Send + Sync + 'static {
    /// Set the maximum DA sizes on the block builder.
    ///
    /// `max_tx_size` — maximum DA bytes allowed per transaction.
    /// `max_block_size` — maximum DA bytes allowed per block.
    fn set_max_da_size(
        &self,
        max_tx_size: u64,
        max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>>;
}

impl<T: ThrottleClient> ThrottleClient for Arc<T> {
    fn set_max_da_size(
        &self,
        max_tx_size: u64,
        max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        (**self).set_max_da_size(max_tx_size, max_block_size)
    }
}

/// No-op [`ThrottleClient`] that silently discards all DA limit calls.
///
/// Used when throttling is disabled so the driver loop requires no special
/// casing: calls to `set_max_da_size` simply return `Ok(())` immediately.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopThrottleClient;

impl ThrottleClient for NoopThrottleClient {
    fn set_max_da_size(
        &self,
        _max_tx_size: u64,
        _max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async { Ok(()) })
    }
}
