//! Driver loop for the proposer.
//!
//! The driver coordinates between RPC clients, the enclave, and contract
//! interactions to generate and submit output proposals.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::B256;
use eyre::Result;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::contracts::OnchainVerifierClient;
use crate::contracts::output_proposer::OutputProposer;
use crate::enclave::EnclaveClientTrait;
use crate::metrics as proposer_metrics;
use crate::prover::{Prover, ProverProposal};
use crate::rpc::{L1Client, L2Client, RollupClient};
use crate::{
    AGGREGATE_BATCH_SIZE, BLOCKHASH_SAFETY_MARGIN, BLOCKHASH_WINDOW, PROPOSAL_TIMEOUT,
    ProposerError,
};

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
pub struct Driver<L1, L2, E, R, V>
where
    L1: L1Client,
    L2: L2Client,
    E: EnclaveClientTrait,
    R: RollupClient,
    V: OnchainVerifierClient,
{
    config: DriverConfig,
    prover: Arc<Prover<L1, L2, E>>,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    rollup_client: Arc<R>,
    verifier_client: Arc<V>,
    output_proposer: Arc<dyn OutputProposer>,
    cancel: CancellationToken,
    /// Pending single-block proposals awaiting aggregation and submission.
    pending: VecDeque<ProverProposal>,
}

impl<L1, L2, E, R, V> std::fmt::Debug for Driver<L1, L2, E, R, V>
where
    L1: L1Client,
    L2: L2Client,
    E: EnclaveClientTrait,
    R: RollupClient,
    V: OnchainVerifierClient,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("config", &self.config)
            .field("pending_count", &self.pending.len())
            .field("has_output_proposer", &true)
            .finish_non_exhaustive()
    }
}

impl<L1, L2, E, R, V> Driver<L1, L2, E, R, V>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
    V: OnchainVerifierClient + 'static,
{
    /// Creates a new driver with the given configuration.
    pub fn new(
        config: DriverConfig,
        prover: Arc<Prover<L1, L2, E>>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
        verifier_client: Arc<V>,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            prover,
            l1_client,
            l2_client,
            rollup_client,
            verifier_client,
            output_proposer,
            cancel,
            pending: VecDeque::new(),
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
                        warn!(error = %e, "Driver step failed");
                    }
                }
            }
        }

        info!("Driver loop stopped");
        Ok(())
    }

    /// Performs a single driver step (one tick of the loop).
    ///
    /// Fetches latest onchain output, generates new proofs, attempts
    /// aggregation, and proposes if conditions are met.
    async fn step(&mut self) -> Result<(), ProposerError> {
        let latest_output = self.verifier_client.latest_output_proposal().await?;
        let latest_output_number: u64 = latest_output.l2BlockNumber.try_into().unwrap_or(u64::MAX);
        let latest_output_root: B256 = latest_output.outputRoot;

        if let Err(e) = self.generate_outputs(latest_output_number).await {
            warn!(error = %e, "Error generating outputs");
            return Err(e);
        }

        match self
            .next_output(latest_output_number, latest_output_root)
            .await
        {
            Ok(Some(proposal)) => {
                self.propose_output(&proposal).await;
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "Error getting next output");
                return Err(e);
            }
        }

        Ok(())
    }

    /// Generates single-block proofs, filling the pending queue.
    ///
    /// Port of Go `generateOutputs()`:
    /// 1. Clears already-submitted proposals from the front of the queue
    /// 2. Validates contiguity with onchain state (clears queue on gap)
    /// 3. Generates up to `AGGREGATE_BATCH_SIZE` new single-block proofs
    /// 4. Stops on `BlockNotFound` (block doesn't exist yet)
    async fn generate_outputs(&mut self, latest_output_number: u64) -> Result<(), ProposerError> {
        let mut next_number = latest_output_number;

        // Clear out already-submitted outputs from front of queue.
        while let Some(front) = self.pending.front() {
            if front.from.number.saturating_sub(1) < latest_output_number {
                self.pending.pop_front();
            } else {
                break;
            }
        }

        // Validate contiguity: the front of the queue should chain from latest output.
        if let Some(front) = self.pending.front() {
            if front.from.number.saturating_sub(1) != latest_output_number {
                warn!(
                    latest = latest_output_number,
                    pending_from = front.from.number.saturating_sub(1),
                    "Pending outputs are not contiguous with the latest output"
                );
                self.pending.clear();
            } else {
                // Continue from end of pending queue.
                next_number = self.pending.back().unwrap().to.number;
            }
        }

        // Generate up to AGGREGATE_BATCH_SIZE new proofs.
        for i in 0..AGGREGATE_BATCH_SIZE {
            let number = next_number + 1 + i as u64;
            let block = match self.l2_client.block_by_number(Some(number)).await {
                Ok(block) => block,
                Err(crate::rpc::RpcError::BlockNotFound(_)) => {
                    // Block doesn't exist yet; stop generating but keep existing pending.
                    break;
                }
                Err(e) => {
                    return Err(ProposerError::Rpc(e));
                }
            };

            let proposal = self.prover.generate(&block).await?;

            // Calculate blocks behind safe head for logging.
            let blocks_behind = match self.latest_safe_block_number().await {
                Ok(safe_number) if safe_number > proposal.to.number => {
                    safe_number - proposal.to.number
                }
                _ => 0,
            };

            info!(
                block_number = proposal.to.number,
                l1_origin_number = proposal.to.l1origin.number,
                l1_origin_hash = ?proposal.to.l1origin.hash,
                has_withdrawals = proposal.has_withdrawals,
                output_root = ?proposal.output.output_root,
                blocks_behind,
                "Generated proof for block"
            );
            self.pending.push_back(proposal);
        }

        debug!(queue_depth = self.pending.len(), "Proof queue depth");
        metrics::gauge!(proposer_metrics::PROOF_QUEUE_DEPTH).set(self.pending.len() as f64);
        Ok(())
    }

    /// Determines the next output to propose, aggregating if needed.
    ///
    /// Port of Go `nextOutput()`:
    /// 1. Counts pending proposals up to latest safe block
    /// 2. Aggregates in batches of `AGGREGATE_BATCH_SIZE`
    /// 3. Checks aggregated output matches latest safe block hash (reorg detection)
    /// 4. Checks `min_proposal_interval` gating
    /// 5. Checks BLOCKHASH window
    async fn next_output(
        &mut self,
        latest_output_number: u64,
        latest_output_root: B256,
    ) -> Result<Option<ProverProposal>, ProposerError> {
        let latest_safe = self.latest_safe_block().await?;
        let latest_safe_number = latest_safe.number;
        let latest_safe_hash = latest_safe.hash;

        // Count pending proposals up to the latest safe block.
        let mut count = 0;
        for p in &self.pending {
            if p.to.number <= latest_safe_number {
                count += 1;
            } else {
                break;
            }
        }
        if count == 0 {
            return Ok(None);
        }

        // Aggregate proposals in batches.
        while count > 1 {
            let batch_length = count.min(AGGREGATE_BATCH_SIZE);
            let batch: Vec<ProverProposal> = self.pending.drain(..batch_length).collect();

            match self.prover.aggregate(latest_output_root, batch).await {
                Ok(aggregated) => {
                    info!(
                        output_root = ?aggregated.output.output_root,
                        blocks = batch_length,
                        remaining = count - batch_length,
                        has_withdrawals = aggregated.has_withdrawals,
                        from = aggregated.from.number,
                        to = aggregated.to.number,
                        "Aggregated proofs"
                    );
                    self.pending.push_front(aggregated);
                    count -= batch_length - 1;
                }
                Err(e) => {
                    if matches!(e, ProposerError::Enclave(_)) {
                        warn!(error = %e, "Non-recoverable error aggregating proofs");
                        self.pending.clear();
                    }
                    return Err(e);
                }
            }
        }

        let proposal = &self.pending[0];

        // Check that the aggregated proposal reaches the latest safe block.
        if proposal.to.number < latest_safe_number {
            info!(
                aggregated_to = proposal.to.number,
                latest_safe = latest_safe_number,
                "Aggregated output is not the latest safe block, waiting for more proofs"
            );
            return Ok(None);
        }

        // Reorg detection: check that the aggregated output matches the safe block hash.
        if proposal.to.hash != latest_safe_hash {
            warn!(
                aggregated_hash = ?proposal.to.hash,
                safe_hash = ?latest_safe_hash,
                block_number = proposal.to.number,
                "Aggregated output does not match the latest safe block, possible reorg"
            );
            self.pending.clear();
            return Ok(None);
        }

        // Check min_proposal_interval gating.
        let should_propose = self.config.min_proposal_interval > 0
            && latest_safe_number.saturating_sub(latest_output_number)
                > self.config.min_proposal_interval;

        if !should_propose {
            debug!(
                safe = latest_safe_number,
                latest_output = latest_output_number,
                min_interval = self.config.min_proposal_interval,
                "Not yet time to propose"
            );
            return Ok(None);
        }

        // Check BLOCKHASH window: L1 origin must be recent enough.
        let l1_latest = match self.l1_client.block_number().await {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "Failed to get latest L1 block number");
                return Ok(None);
            }
        };

        let l1_origin_number = proposal.to.l1origin.number;
        if l1_origin_number <= l1_latest.saturating_sub(BLOCKHASH_WINDOW - BLOCKHASH_SAFETY_MARGIN)
        {
            warn!(
                l1_origin = l1_origin_number,
                l1_latest, "Not submitting proposal, L1 origin block is too old"
            );
            return Ok(None);
        }

        // Clone the proposal to return it.
        Ok(Some(self.pending[0].clone()))
    }

    /// Returns the latest safe L2 block reference.
    ///
    /// Uses `safe_l2` if `allow_non_finalized` is set, otherwise `finalized_l2`.
    async fn latest_safe_block(&self) -> Result<crate::rpc::L2BlockRef, ProposerError> {
        let sync_status = self.rollup_client.sync_status().await?;
        if self.config.allow_non_finalized {
            Ok(sync_status.safe_l2)
        } else {
            Ok(sync_status.finalized_l2)
        }
    }

    /// Returns the latest safe L2 block number.
    async fn latest_safe_block_number(&self) -> Result<u64, ProposerError> {
        self.latest_safe_block().await.map(|b| b.number)
    }

    /// Submits a proposal to L1 via the output proposer.
    ///
    /// Wraps the submission in a [`PROPOSAL_TIMEOUT`] and logs success/failure/timeout.
    /// Does NOT pop from pending (cleanup happens in `generate_outputs` on next tick).
    async fn propose_output(&self, proposal: &ProverProposal) {
        info!(
            l2_block_number = proposal.to.number,
            l1_origin_number = proposal.to.l1origin.number,
            l1_origin_hash = ?proposal.to.l1origin.hash,
            output_root = ?proposal.output.output_root,
            has_withdrawals = proposal.has_withdrawals,
            from = proposal.from.number,
            to = proposal.to.number,
            "Proposing output"
        );

        match tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer.propose_output(proposal),
        )
        .await
        {
            Ok(Ok(())) => {
                info!(
                    l2_block_number = proposal.to.number,
                    "Output proposal submitted successfully"
                );
                metrics::counter!(proposer_metrics::L2_OUTPUT_PROPOSALS_TOTAL).increment(1);
            }
            Ok(Err(e)) => {
                warn!(
                    error = %e,
                    l2_block_number = proposal.to.number,
                    "Failed to submit output proposal"
                );
            }
            Err(_) => {
                warn!(
                    l2_block_number = proposal.to.number,
                    timeout_secs = PROPOSAL_TIMEOUT.as_secs(),
                    "Output proposal timed out"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{B256, Bytes};
    use async_trait::async_trait;
    use op_enclave_client::{ClientError, ExecuteStatelessRequest};
    use op_enclave_core::Proposal;
    use op_enclave_core::types::config::RollupConfig;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::enclave::EnclaveClientTrait;
    use crate::prover::Prover;
    use crate::prover::types::test_helpers::test_proposal;
    use crate::rpc::SyncStatus;
    use crate::test_utils::{
        MockL1, MockL2, MockOnchainVerifier, MockOutputProposer, MockRollupClient,
        test_output_proposal, test_per_chain_config, test_sync_status,
    };

    // ---- Mock infrastructure ----

    /// Mock enclave (methods `unimplemented!()`) — not called in these tests.
    struct MockEnclave;

    #[async_trait]
    impl EnclaveClientTrait for MockEnclave {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
        async fn aggregate(
            &self,
            _: B256,
            _: B256,
            _: Vec<Proposal>,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
    }

    /// Mock enclave that returns a valid `Proposal` from `aggregate()`.
    /// Used for testing the aggregation code path in `next_output()`.
    struct MockEnclaveForAggregation;

    #[async_trait]
    impl EnclaveClientTrait for MockEnclaveForAggregation {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
        async fn aggregate(
            &self,
            _: B256,
            _: B256,
            proposals: Vec<Proposal>,
        ) -> Result<Proposal, ClientError> {
            // Return a proposal based on the last input proposal's fields.
            let last = proposals.last().unwrap();
            Ok(Proposal {
                output_root: last.output_root,
                signature: Bytes::from(vec![0xab; 65]),
                l1_origin_hash: last.l1_origin_hash,
                l2_block_number: last.l2_block_number,
                prev_output_root: last.prev_output_root,
                config_hash: last.config_hash,
            })
        }
    }

    // ---- Helpers ----

    /// Generic driver constructor that accepts a custom enclave, config, and cancel token.
    fn test_driver_custom<E: EnclaveClientTrait + 'static>(
        enclave: E,
        driver_config: DriverConfig,
        l1_block_number: u64,
        sync_status: SyncStatus,
        output_proposal: crate::contracts::OutputProposal,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Driver<MockL1, MockL2, E, MockRollupClient, MockOnchainVerifier> {
        let l1 = Arc::new(MockL1 {
            latest_block_number: l1_block_number,
        });
        let l2 = Arc::new(MockL2 {
            block_not_found: true,
        });
        let prover = Arc::new(Prover::new(
            test_per_chain_config(),
            RollupConfig::default(),
            Arc::clone(&l1),
            Arc::clone(&l2),
            enclave,
        ));
        let rollup = Arc::new(MockRollupClient { sync_status });
        let verifier = Arc::new(MockOnchainVerifier { output_proposal });

        Driver::new(
            driver_config,
            prover,
            l1,
            l2,
            rollup,
            verifier,
            output_proposer,
            cancel,
        )
    }

    fn test_driver(
        l1_block_number: u64,
        sync_status: SyncStatus,
        output_proposal: crate::contracts::OutputProposal,
    ) -> Driver<MockL1, MockL2, MockEnclave, MockRollupClient, MockOnchainVerifier> {
        test_driver_custom(
            MockEnclave,
            DriverConfig::default(),
            l1_block_number,
            sync_status,
            output_proposal,
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        )
    }

    // ---- Tests ----

    #[tokio::test]
    async fn test_generate_outputs_clears_submitted() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let output = test_output_proposal(10);
        let mut driver = test_driver(1000, sync_status, output);

        // Pre-populate pending with blocks 5..=12
        for n in 5..=12 {
            driver.pending.push_back(test_proposal(n, n, false));
        }
        assert_eq!(driver.pending.len(), 8);

        // generate_outputs(10) should drain blocks 5..=10, keep 11..=12
        let result = driver.generate_outputs(10).await;
        assert!(result.is_ok());

        // Blocks 5-10 (from.number - 1 < 10) should be drained
        // Remaining: blocks 11, 12
        assert_eq!(driver.pending.len(), 2);
        assert_eq!(driver.pending[0].from.number, 11);
        assert_eq!(driver.pending[1].from.number, 12);
    }

    #[tokio::test]
    async fn test_generate_outputs_clears_on_gap() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let output = test_output_proposal(10);
        let mut driver = test_driver(1000, sync_status, output);

        // Pre-populate with blocks 15..=17 (gap: 10 -> 15)
        for n in 15..=17 {
            driver.pending.push_back(test_proposal(n, n, false));
        }
        assert_eq!(driver.pending.len(), 3);

        // generate_outputs(10) — front is block 15 which doesn't chain from 10
        // Queue should be cleared entirely
        let result = driver.generate_outputs(10).await;
        assert!(result.is_ok());
        // Queue cleared due to gap, then BlockNotFound stops new generation
        assert_eq!(driver.pending.len(), 0);
    }

    #[tokio::test]
    async fn test_next_output_returns_none_when_empty() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let output = test_output_proposal(100);
        let mut driver = test_driver(1000, sync_status, output);

        // Empty pending queue
        let result = driver.next_output(100, B256::ZERO).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_next_output_reorg_detection() {
        // Pending has one proposal for block 105 with to.hash = 0x30..
        // Safe block hash = 0xBB.. (mismatch)
        let safe_hash = B256::repeat_byte(0xBB);
        let sync_status = test_sync_status(105, safe_hash);
        let output = test_output_proposal(100);
        let mut driver = test_driver(1000, sync_status, output);

        // test_proposal sets to.hash = B256::repeat_byte(0x30) which != 0xBB
        driver.pending.push_back(test_proposal(105, 105, false));
        assert_eq!(driver.pending.len(), 1);

        let result = driver.next_output(100, B256::ZERO).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        // Queue should be cleared due to reorg detection
        assert_eq!(driver.pending.len(), 0);
    }

    #[tokio::test]
    async fn test_next_output_min_interval_gating() {
        // latest_output=100, safe=200, min_proposal_interval=512 (default)
        // Gap = 200 - 100 = 100, which is <= 512, so should NOT propose
        let safe_hash = B256::repeat_byte(0x30); // Matches test_proposal's to.hash
        let sync_status = test_sync_status(200, safe_hash);
        let output = test_output_proposal(100);
        let mut driver = test_driver(1000, sync_status, output);

        driver.pending.push_back(test_proposal(200, 200, false));

        let result = driver.next_output(100, B256::ZERO).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        // Queue should NOT be cleared — min_interval gating doesn't clear the queue
        assert_eq!(driver.pending.len(), 1);
    }

    #[tokio::test]
    async fn test_next_output_blockhash_window() {
        // l1_latest=1000, proposal l1origin.number=700
        // Threshold = 1000 - (256-10) = 754
        // 700 <= 754, so should NOT propose (L1 origin too old)
        //
        // Must pass min_proposal_interval first:
        // latest_output=100, safe=800, gap=700 > 512 ✓
        let safe_hash = B256::repeat_byte(0x30); // Matches test_proposal's to.hash
        let sync_status = test_sync_status(800, safe_hash);
        let output = test_output_proposal(100);
        let mut driver = test_driver(1000, sync_status, output);

        // Create a proposal where to.l1origin.number = 700
        let mut proposal = test_proposal(800, 800, false);
        proposal.to.l1origin.number = 700;
        driver.pending.push_back(proposal);

        let result = driver.next_output(100, B256::ZERO).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_next_output_happy_path() {
        // Setup: latest_output=100, safe=700, safe_hash=0x30 (matches test_proposal's to.hash)
        let safe_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(700, safe_hash);
        let output = test_output_proposal(100);
        let mut driver = test_driver(1000, sync_status, output);

        // Push a proposal: from=101, to=700, l1origin.number = 100+700 = 800
        driver.pending.push_back(test_proposal(101, 700, false));
        assert_eq!(driver.pending.len(), 1);

        // All gates should pass:
        // - count=1 (block 700 <= safe 700) ✓
        // - hash match: to.hash=0x30 == safe_hash=0x30 ✓
        // - min_proposal_interval: gap=700-100=600 > 512 ✓
        // - BLOCKHASH: l1origin=800 > 1000-(256-10)=754 ✓
        let result = driver.next_output(100, B256::ZERO).await;
        assert!(result.is_ok());
        let proposal = result.unwrap();
        assert!(proposal.is_some(), "expected Some(proposal)");
        let proposal = proposal.unwrap();
        assert_eq!(proposal.from.number, 101);
        assert_eq!(proposal.to.number, 700);
        // Pending should still have the item (next_output clones, doesn't pop)
        assert_eq!(driver.pending.len(), 1);
    }

    #[tokio::test]
    async fn test_step_happy_path() {
        // Same setup as happy path: latest_output=100, safe=700, l1_latest=1000
        let safe_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(700, safe_hash);
        let output = test_output_proposal(100);
        let mut driver = test_driver(1000, sync_status, output);

        // Pre-populate pending with a contiguous proposal
        driver.pending.push_back(test_proposal(101, 700, false));

        // step() will:
        // 1. generate_outputs(100): keeps proposal (contiguous), hits BlockNotFound on 701
        // 2. next_output returns Some (all gates pass)
        // 3. propose_output logs the proposal
        let result = driver.step().await;
        assert!(result.is_ok(), "step() should succeed, got: {result:?}");
        // Pending still has 1 item (generate_outputs keeps contiguous, propose_output doesn't pop)
        assert_eq!(driver.pending.len(), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_run_cancellation() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let output = test_output_proposal(100);
        let cancel = CancellationToken::new();

        let mut driver = test_driver_custom(
            MockEnclave,
            DriverConfig {
                poll_interval: Duration::from_secs(3600), // long interval so we don't step
                ..Default::default()
            },
            1000,
            sync_status,
            output,
            Arc::new(MockOutputProposer),
            cancel.clone(),
        );

        let handle = tokio::spawn(async move { driver.run().await });

        // Cancel immediately — with start_paused, tokio time is virtual
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok(), "run() should return Ok on cancellation");
    }

    #[tokio::test]
    async fn test_step_calls_output_proposer() {
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Tracking proposer that records whether `propose_output` was called.
        struct TrackingOutputProposer {
            called: AtomicBool,
        }

        #[async_trait]
        impl OutputProposer for TrackingOutputProposer {
            async fn propose_output(
                &self,
                _proposal: &ProverProposal,
            ) -> Result<(), ProposerError> {
                self.called.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let safe_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(700, safe_hash);
        let output = test_output_proposal(100);
        let tracking = Arc::new(TrackingOutputProposer {
            called: AtomicBool::new(false),
        });

        let mut driver = test_driver_custom(
            MockEnclave,
            DriverConfig::default(),
            1000,
            sync_status,
            output,
            Arc::clone(&tracking) as Arc<dyn OutputProposer>,
            CancellationToken::new(),
        );

        driver.pending.push_back(test_proposal(101, 700, false));

        let result = driver.step().await;
        assert!(result.is_ok(), "step() should succeed, got: {result:?}");
        assert!(
            tracking.called.load(Ordering::SeqCst),
            "OutputProposer::propose_output should have been called"
        );
    }

    #[tokio::test]
    async fn test_next_output_with_aggregation() {
        let safe_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(103, safe_hash);
        let output = test_output_proposal(100);
        let cancel = CancellationToken::new();

        let mut driver = test_driver_custom(
            MockEnclaveForAggregation,
            DriverConfig {
                min_proposal_interval: 1, // low threshold so interval gate passes
                ..Default::default()
            },
            400, // l1_latest=400
            sync_status,
            output,
            Arc::new(MockOutputProposer),
            cancel,
        );

        // Pre-populate 3 single-block proposals: blocks 101, 102, 103
        driver.pending.push_back(test_proposal(101, 101, false));
        driver.pending.push_back(test_proposal(102, 102, false));
        driver.pending.push_back(test_proposal(103, 103, true));
        assert_eq!(driver.pending.len(), 3);

        // next_output should:
        // - count=3 (all <= safe 103)
        // - aggregate 3 proposals into 1 (since count > 1)
        // - hash match: aggregated to.hash = 0x30 == safe_hash ✓
        // - min_proposal_interval: gap=103-100=3 > 1 ✓
        // - BLOCKHASH: l1origin = 100+103 = 203 > 400-(256-10) = 154 ✓
        let result = driver.next_output(100, B256::ZERO).await;
        assert!(result.is_ok());
        let proposal = result.unwrap();
        assert!(
            proposal.is_some(),
            "expected Some(proposal) after aggregation"
        );
        let proposal = proposal.unwrap();
        assert_eq!(proposal.from.number, 101);
        assert_eq!(proposal.to.number, 103);
        // Pending should have 1 aggregated item
        assert_eq!(driver.pending.len(), 1);
    }
}
