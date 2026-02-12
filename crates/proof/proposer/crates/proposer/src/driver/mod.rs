//! Driver loop for the proposer.
//!
//! The driver coordinates between RPC clients, the enclave, and contract
//! interactions to generate and submit output proposals.

use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::enclave::EnclaveClientTrait;
use crate::prover::{Prover, ProverProposal};
use crate::rpc::{L1Client, L2Client, RollupClient};

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// Minimum number of blocks between proposals.
    pub min_proposal_interval: u64,
    /// If true, use `safe_l2` (derived from L1 but L1 not yet finalized).
    /// If false (default), use `finalized_l2` (derived from finalized L1).
    pub allow_non_finalized: bool,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(12),
            min_proposal_interval: 512,
            allow_non_finalized: false,
        }
    }
}

/// The main driver that coordinates proposal generation.
#[derive(Debug)]
pub struct Driver<L1, L2, E, R>
where
    L1: L1Client,
    L2: L2Client,
    E: EnclaveClientTrait,
    R: RollupClient,
{
    config: DriverConfig,
    prover: Arc<Prover<L1, L2, E>>,
    l2_client: Arc<L2>,
    rollup_client: Arc<R>,
    cancel: CancellationToken,
    /// The last proposed L2 block number.
    last_proposed_block: Option<u64>,
}

impl<L1, L2, E, R> Driver<L1, L2, E, R>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
{
    /// Creates a new driver with the given configuration.
    pub fn new(
        config: DriverConfig,
        prover: Arc<Prover<L1, L2, E>>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
    ) -> Self {
        Self {
            config,
            prover,
            l2_client,
            rollup_client,
            cancel: CancellationToken::new(),
            last_proposed_block: None,
        }
    }

    /// Starts the driver loop.
    pub async fn run(&mut self) -> Result<()> {
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
    ///
    /// This checks if we need to propose a new block, and if so,
    /// generates a proposal using the prover.
    async fn step(&mut self) -> Result<()> {
        // Get sync status to find the target L2 block
        let sync_status = self.rollup_client.sync_status().await?;
        let target_l2 = if self.config.allow_non_finalized {
            &sync_status.safe_l2
        } else {
            &sync_status.finalized_l2
        };
        let target_l2_number = target_l2.number;

        // Check if we need to propose
        let should_propose = self
            .last_proposed_block
            .is_none_or(|last| target_l2_number >= last + self.config.min_proposal_interval);

        if !should_propose {
            debug!(
                target_l2 = target_l2_number,
                last_proposed = ?self.last_proposed_block,
                min_interval = self.config.min_proposal_interval,
                "Not yet time to propose"
            );
            return Ok(());
        }

        // Get the block to propose
        let block = self
            .l2_client
            .block_by_number(Some(target_l2_number))
            .await?;

        info!(
            block_number = block.header.number,
            block_hash = ?block.header.hash,
            "Generating proposal for block"
        );

        // Generate the proposal
        match self.prover.generate(&block).await {
            Ok(proposal) => {
                self.handle_proposal(proposal).await?;
                self.last_proposed_block = Some(block.header.number);
            }
            Err(e) => {
                error!(
                    block_number = block.header.number,
                    error = %e,
                    "Failed to generate proposal"
                );
                return Err(e.into());
            }
        }

        Ok(())
    }

    /// Handles a generated proposal.
    ///
    /// This is where we would submit the proposal to L1 or aggregate
    /// multiple proposals. For now, we just log the proposal.
    async fn handle_proposal(&self, proposal: ProverProposal) -> Result<()> {
        info!(
            l2_block_number = proposal.to.number,
            l1_origin_number = proposal.to.l1origin.number,
            l1_origin_hash = ?proposal.to.l1origin.hash,
            has_withdrawals = proposal.has_withdrawals,
            output_root = ?proposal.output.output_root,
            "Generated proposal"
        );

        // TODO: Submit proposal to L1 contract or aggregate with other proposals

        Ok(())
    }

    /// Returns a cancellation token that can be used to stop the driver from another task.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
