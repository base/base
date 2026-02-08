//! Driver loop for the proposer.
//!
//! The driver coordinates between RPC clients, the enclave, and contract
//! interactions to generate and submit output proposals.

use std::time::Duration;

use eyre::Result;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(12),
        }
    }
}

/// The main driver that coordinates proposal generation.
#[derive(Debug)]
pub struct Driver {
    config: DriverConfig,
    cancel: CancellationToken,
}

impl Driver {
    /// Creates a new driver with the given configuration.
    pub fn new(config: DriverConfig) -> Self {
        Self {
            config,
            cancel: CancellationToken::new(),
        }
    }

    /// Starts the driver loop.
    pub async fn run(&self) -> Result<()> {
        info!("Starting driver loop");

        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    info!("Driver received shutdown signal");
                    break;
                }
                () = sleep(self.config.poll_interval) => {
                    if let Err(e) = self.step().await {
                        warn!("Driver step failed: {e}");
                    }
                }
            }
        }

        info!("Driver loop stopped");
        Ok(())
    }

    /// Performs a single driver step.
    async fn step(&self) -> Result<()> {
        // TODO: Implement the actual driver logic:
        // 1. Check if we need to propose (based on min_proposal_interval)
        // 2. Get the latest L2 block from rollup node
        // 3. Request proof from enclave
        // 4. Submit proposal to L1 contract
        debug!("Driver step - not yet implemented");
        Ok(())
    }

    /// Returns a cancellation token that can be used to stop the driver from another task.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
