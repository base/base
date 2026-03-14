//! Batcher service startup and wiring.

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::BatcherConfig;

/// The batcher service.
///
/// Wires the encoder, block source, transaction manager, and driver
/// into a running batcher process. Call [`start`](Self::start) to run.
#[derive(Debug)]
pub struct BatcherService {
    /// Full batcher configuration.
    config: BatcherConfig,
}

impl BatcherService {
    /// Create a new [`BatcherService`] from the given configuration.
    pub const fn new(config: BatcherConfig) -> Self {
        Self { config }
    }

    /// Start the batcher service.
    ///
    /// This runs until the cancellation token is triggered. Full RPC wiring
    /// (L2 WebSocket subscriptions, alloy providers, `SimpleTxManager`) is
    /// deferred to a follow-up task; this stub compiles and validates the
    /// config/CLI integration.
    pub async fn start(self, cancellation: CancellationToken) -> eyre::Result<()> {
        info!(
            inbox = %self.config.inbox_address,
            l1_rpc = %self.config.l1_rpc_url,
            l2_rpc = %self.config.l2_rpc_url,
            "starting batcher service"
        );

        info!("batcher service components initialized");
        cancellation.cancelled().await;
        info!("batcher service shutting down");
        Ok(())
    }
}
