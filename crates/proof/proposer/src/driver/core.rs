//! Core driver logic for the proposer.
//!
//! The [`Driver`] runs a single polling loop that recovers the latest on-chain
//! game state, proves the next block range via the prover server, checks for
//! reorgs, and submits the output as a dispute game.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{B256, U256};
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient,
};
use base_proof_primitives::{ProofRequest, ProofResult, Proposal, ProverClient};
use base_proof_rpc::{L1Provider, L2BlockRef, L2Provider, RollupProvider};
use eyre::Result;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    constants::{MAX_GAME_RECOVERY_LOOKBACK, NO_PARENT_INDEX, PROPOSAL_TIMEOUT},
    error::ProposerError,
    metrics as proposer_metrics,
    output_proposer::{OutputProposer, is_game_already_exists},
};

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// Number of L2 blocks between proposals (read from `AggregateVerifier` at startup).
    pub block_interval: u64,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// ETH bond required to create a dispute game.
    pub init_bond: U256,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// If true, use `safe_l2` (derived from L1 but L1 not yet finalized).
    /// If false (default), use `finalized_l2` (derived from finalized L1).
    pub allow_non_finalized: bool,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(12),
            block_interval: 512,
            intermediate_block_interval: 512,
            init_bond: U256::ZERO,
            game_type: 0,
            allow_non_finalized: false,
        }
    }
}

/// On-chain state recovered by [`Driver::recover_latest_state`].
///
/// This is either a game found in the `DisputeGameFactory` or the
/// anchor root from the `AnchorStateRegistry` when no games exist.
#[derive(Debug, Clone)]
pub struct RecoveredState {
    /// Factory index of the game, or [`NO_PARENT_INDEX`] for anchor state.
    pub game_index: u32,
    /// Output root claimed by the game or anchor state.
    pub output_root: B256,
    /// L2 block number of the claim.
    pub l2_block_number: u64,
}

/// The main driver that coordinates proposal generation.
pub struct Driver<L1, L2, R, ASR, F>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    config: DriverConfig,
    prover: Arc<dyn ProverClient>,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    rollup_client: Arc<R>,
    anchor_registry: Arc<ASR>,
    factory_client: Arc<F>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    output_proposer: Arc<dyn OutputProposer>,
    cancel: CancellationToken,
}

impl<L1, L2, R, ASR, F> std::fmt::Debug for Driver<L1, L2, R, ASR, F>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<L1, L2, R, ASR, F> Driver<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    /// Creates a new driver with the given configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: DriverConfig,
        prover: Arc<dyn ProverClient>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
        anchor_registry: Arc<ASR>,
        factory_client: Arc<F>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            prover,
            l1_client,
            l2_client,
            rollup_client,
            anchor_registry,
            factory_client,
            verifier_client,
            output_proposer,
            cancel,
        }
    }

    /// Replaces the cancellation token.
    ///
    /// Used by [`super::DriverHandle`] to create fresh sessions when the driver
    /// is restarted via the admin RPC.
    pub fn set_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    /// Scans the `DisputeGameFactory` on-chain to find the most recent game
    /// of our `game_type`.
    ///
    /// Recovers the latest on-chain state to use as the parent for the next proposal.
    ///
    /// Walks the `DisputeGameFactory` backwards for a matching game, falling
    /// back to the `AnchorStateRegistry` when none is found.
    async fn recover_latest_state(&self) -> Result<RecoveredState, ProposerError> {
        let count = self
            .factory_client
            .game_count()
            .await
            .map_err(|e| ProposerError::Contract(format!("recovery game_count failed: {e}")))?;

        let search_count = count.min(MAX_GAME_RECOVERY_LOOKBACK);
        for i in 0..search_count {
            let game_index = count - 1 - i;
            let game = match self.factory_client.game_at_index(game_index).await {
                Ok(g) => g,
                Err(e) => {
                    warn!(error = %e, game_index, "Failed to read game at index during recovery");
                    continue;
                }
            };

            if game.game_type != self.config.game_type {
                continue;
            }

            let game_info = match self.verifier_client.game_info(game.proxy).await {
                Ok(info) => info,
                Err(e) => {
                    warn!(error = %e, game_index, "Failed to read game_info during recovery");
                    continue;
                }
            };

            let idx: u32 = game_index.try_into().map_err(|_| {
                ProposerError::Contract(format!("game index {game_index} exceeds u32"))
            })?;

            debug!(
                game_index,
                game_proxy = %game.proxy,
                output_root = ?game_info.root_claim,
                l2_block_number = game_info.l2_block_number,
                "Recovered parent game state from on-chain"
            );

            return Ok(RecoveredState {
                game_index: idx,
                output_root: game_info.root_claim,
                l2_block_number: game_info.l2_block_number,
            });
        }

        debug!(
            game_type = self.config.game_type,
            searched = search_count,
            "No games found for our game type, falling back to anchor state registry"
        );
        self.recover_from_anchor_registry().await
    }

    /// Builds a [`RecoveredState`] from the anchor state registry.
    async fn recover_from_anchor_registry(&self) -> Result<RecoveredState, ProposerError> {
        let anchor = self.anchor_registry.get_anchor_root().await?;
        debug!(
            l2_block_number = anchor.l2_block_number,
            root = ?anchor.root,
            "Recovered state from anchor state registry"
        );
        Ok(RecoveredState {
            game_index: NO_PARENT_INDEX,
            output_root: anchor.root,
            l2_block_number: anchor.l2_block_number,
        })
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
                        warn!(error = %e, "Driver step failed");
                    }
                }
            }
        }

        info!("Driver loop stopped");
        Ok(())
    }

    /// Performs a single driver step (one tick of the loop).
    async fn step(&self) -> Result<(), ProposerError> {
        // Load parent state from chain to stay in sync with other proposers.
        let state = self.recover_latest_state().await?;
        let starting_block_number = state.l2_block_number;
        let agreed_l2_output_root = state.output_root;
        let parent_index = state.game_index;

        // Compute the target block for this interval.
        let target_block = starting_block_number
            .checked_add(self.config.block_interval)
            .ok_or_else(|| ProposerError::Internal("overflow computing target block".into()))?;

        // Fetch the safe head and ensure the target is safe before proving.
        let latest_safe = self.latest_safe_block().await?;
        if target_block > latest_safe.number {
            debug!(
                target_block,
                safe_head = latest_safe.number,
                "Target block not yet safe, skipping"
            );
            return Ok(());
        }

        // Gather the data needed to build a ProofRequest.
        let agreed_l2_head = self
            .l2_client
            .header_by_number(Some(starting_block_number))
            .await
            .map_err(ProposerError::Rpc)?;

        let claimed_output =
            self.rollup_client.output_at_block(target_block).await.map_err(ProposerError::Rpc)?;

        let l1_head = self.l1_client.header_by_number(None).await.map_err(ProposerError::Rpc)?;

        let request = ProofRequest {
            l1_head: l1_head.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root,
            claimed_l2_output_root: claimed_output.output_root,
            claimed_l2_block_number: target_block,
        };

        info!(
            starting_block = starting_block_number,
            target_block,
            l1_head = ?l1_head.hash,
            "Sending proof request to prover"
        );

        let proof_result =
            self.prover.prove(request).await.map_err(|e| ProposerError::Prover(e.to_string()))?;

        let (aggregate_proposal, proposals) = match proof_result {
            ProofResult::Tee { aggregate_proposal, proposals } => (aggregate_proposal, proposals),
            ProofResult::Zk { .. } => {
                return Err(ProposerError::Prover(
                    "unexpected ZK proof result from TEE prover".into(),
                ));
            }
        };

        // Reorg detection: verify the claimed output root matches the canonical chain.
        let canonical_output =
            self.rollup_client.output_at_block(target_block).await.map_err(ProposerError::Rpc)?;

        if aggregate_proposal.output_root != canonical_output.output_root {
            warn!(
                proposal_root = ?aggregate_proposal.output_root,
                canonical_root = ?canonical_output.output_root,
                target_block,
                "Proposal output root does not match canonical chain, possible reorg"
            );
            return Ok(());
        }

        // Extract intermediate roots from per-block proposals.
        let intermediate_roots =
            self.extract_intermediate_roots(starting_block_number, &proposals)?;

        info!(
            target_block,
            output_root = ?aggregate_proposal.output_root,
            parent_index,
            intermediate_roots_count = intermediate_roots.len(),
            proposals_count = proposals.len(),
            "Proposing output (creating dispute game)"
        );

        self.propose_output(&aggregate_proposal, target_block, parent_index, &intermediate_roots)
            .await;

        Ok(())
    }

    /// Extracts intermediate output roots from the per-block proposals returned
    /// by the prover server.
    ///
    /// The intermediate roots are the output roots at every `intermediate_block_interval`
    /// within the proposal range.
    fn extract_intermediate_roots(
        &self,
        starting_block_number: u64,
        proposals: &[Proposal],
    ) -> Result<Vec<B256>, ProposerError> {
        let interval = self.config.intermediate_block_interval;
        if interval == 0 {
            return Err(ProposerError::Config(
                "intermediate_block_interval must not be zero".into(),
            ));
        }
        let count = self.config.block_interval / interval;
        let mut roots = Vec::with_capacity(count as usize);
        for i in 1..=count {
            let target_block = starting_block_number
                .checked_add(i.checked_mul(interval).ok_or_else(|| {
                    ProposerError::Internal("overflow computing intermediate root target".into())
                })?)
                .ok_or_else(|| {
                    ProposerError::Internal("overflow computing intermediate root target".into())
                })?;

            // Proposals are indexed from block starting_block_number+1,
            // so proposal index = target_block - starting_block_number - 1.
            let idx = target_block.saturating_sub(starting_block_number).saturating_sub(1);
            if let Some(p) = proposals.get(idx as usize) {
                roots.push(p.output_root);
            } else {
                return Err(ProposerError::Internal(format!(
                    "intermediate root at block {target_block} not found in proposals (index {idx}, len {})",
                    proposals.len()
                )));
            }
        }
        Ok(roots)
    }

    /// Returns the latest safe L2 block reference.
    async fn latest_safe_block(&self) -> Result<L2BlockRef, ProposerError> {
        let sync_status = self.rollup_client.sync_status().await?;
        if self.config.allow_non_finalized {
            Ok(sync_status.safe_l2)
        } else {
            Ok(sync_status.finalized_l2)
        }
    }

    /// Submits a proposal by creating a dispute game via the factory.
    async fn propose_output(
        &self,
        proposal: &Proposal,
        l2_block_number: u64,
        parent_index: u32,
        intermediate_roots: &[B256],
    ) {
        match tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer.propose_output(
                proposal,
                l2_block_number,
                parent_index,
                intermediate_roots,
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                info!(l2_block_number, "Dispute game created successfully");
                metrics::counter!(proposer_metrics::L2_OUTPUT_PROPOSALS_TOTAL).increment(1);
            }
            Ok(Err(e)) => {
                if is_game_already_exists(&e) {
                    info!(
                        l2_block_number,
                        "Game already exists, next tick will load fresh state from chain"
                    );
                } else {
                    warn!(
                        error = %e,
                        l2_block_number,
                        "Failed to create dispute game"
                    );
                }
            }
            Err(_) => {
                warn!(
                    l2_block_number,
                    timeout_secs = PROPOSAL_TIMEOUT.as_secs(),
                    "Dispute game creation timed out"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes, U256};
    use async_trait::async_trait;
    use base_proof_contracts::GameAtIndex;
    use base_proof_rpc::SyncStatus;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
        MockOutputProposer, MockRollupClient, test_anchor_root, test_prover, test_sync_status,
    };

    fn test_proposal(block_number: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(block_number as u8),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: B256::repeat_byte(0x02),
            l1_origin_number: U256::from(100 + block_number),
            l2_block_number: U256::from(block_number),
            prev_output_root: B256::repeat_byte(0x03),
            config_hash: B256::repeat_byte(0x04),
        }
    }

    fn test_driver_custom(
        driver_config: DriverConfig,
        l1_block_number: u64,
        sync_status: SyncStatus,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Driver<MockL1, MockL2, MockRollupClient, MockAnchorStateRegistry, MockDisputeGameFactory>
    {
        let l1 = Arc::new(MockL1 { latest_block_number: l1_block_number });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover = test_prover();
        let rollup = Arc::new(MockRollupClient { sync_status });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) });
        let factory = Arc::new(MockDisputeGameFactory { game_count: 1 });

        Driver::new(
            driver_config,
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier),
            output_proposer,
            cancel,
        )
    }

    #[test]
    fn test_extract_intermediate_roots_full() {
        let driver = test_driver_custom(
            DriverConfig {
                block_interval: 10,
                intermediate_block_interval: 5,
                ..Default::default()
            },
            1000,
            test_sync_status(200, B256::ZERO),
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        );

        let root_a = B256::repeat_byte(0xAA);
        let root_b = B256::repeat_byte(0xBB);

        let mut proposals: Vec<Proposal> = (101..=110).map(test_proposal).collect();
        proposals[4].output_root = root_a;
        proposals[9].output_root = root_b;

        let roots = driver.extract_intermediate_roots(100, &proposals).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], root_a);
        assert_eq!(roots[1], root_b);
    }

    #[test]
    fn test_extract_intermediate_roots_partial_errors() {
        let driver = test_driver_custom(
            DriverConfig {
                block_interval: 10,
                intermediate_block_interval: 5,
                ..Default::default()
            },
            1000,
            test_sync_status(200, B256::ZERO),
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        );

        // Only 5 proposals (blocks 101-105) but block_interval=10 requires
        // intermediate roots at blocks 105 and 110. Block 110 is at index 9
        // which doesn't exist — should return an error.
        let proposals: Vec<Proposal> = (101..=105).map(test_proposal).collect();

        let err = driver.extract_intermediate_roots(100, &proposals).unwrap_err();
        assert!(matches!(err, ProposerError::Internal(_)), "expected Internal error, got: {err:?}");
    }

    #[test]
    fn test_extract_intermediate_roots_interval_equals_block_interval() {
        let driver = test_driver_custom(
            DriverConfig {
                block_interval: 10,
                intermediate_block_interval: 10,
                ..Default::default()
            },
            1000,
            test_sync_status(200, B256::ZERO),
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        );

        let final_root = B256::repeat_byte(0xFF);
        let mut proposals: Vec<Proposal> = (101..=110).map(test_proposal).collect();
        proposals[9].output_root = final_root;

        let roots = driver.extract_intermediate_roots(100, &proposals).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], final_root);
    }

    #[test]
    fn test_extract_intermediate_roots_zero_interval_errors() {
        let driver = test_driver_custom(
            DriverConfig {
                block_interval: 10,
                intermediate_block_interval: 0,
                ..Default::default()
            },
            1000,
            test_sync_status(200, B256::ZERO),
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        );

        let proposals: Vec<Proposal> = (101..=110).map(test_proposal).collect();
        let result = driver.extract_intermediate_roots(100, &proposals);
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_run_cancellation() {
        let cancel = CancellationToken::new();
        let driver = test_driver_custom(
            DriverConfig {
                poll_interval: Duration::from_secs(3600),
                block_interval: 10,
                ..Default::default()
            },
            1000,
            test_sync_status(200, B256::ZERO),
            Arc::new(MockOutputProposer),
            cancel.clone(),
        );

        let handle = tokio::spawn(async move { driver.run().await });
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok(), "run() should return Ok on cancellation");
    }

    // -----------------------------------------------------------------------
    // Recovery tests
    // -----------------------------------------------------------------------

    struct RecoveryMockFactory {
        games: Vec<(u32, Address)>,
        error_indices: Vec<u64>,
    }

    #[async_trait]
    impl DisputeGameFactoryClient for RecoveryMockFactory {
        async fn game_count(&self) -> Result<u64, base_proof_contracts::ContractError> {
            Ok(self.games.len() as u64)
        }
        async fn game_at_index(
            &self,
            index: u64,
        ) -> Result<GameAtIndex, base_proof_contracts::ContractError> {
            if self.error_indices.contains(&index) {
                return Err(base_proof_contracts::ContractError::Validation(
                    "simulated RPC error".into(),
                ));
            }
            let (game_type, proxy) = self.games[index as usize];
            Ok(GameAtIndex { game_type, timestamp: 0, proxy })
        }
        async fn init_bonds(&self, _: u32) -> Result<U256, base_proof_contracts::ContractError> {
            Ok(U256::ZERO)
        }
        async fn game_impls(&self, _: u32) -> Result<Address, base_proof_contracts::ContractError> {
            Ok(Address::ZERO)
        }
    }

    struct RecoveryMockVerifier {
        l2_block_number: u64,
        root_claim: B256,
        error_proxies: Vec<Address>,
    }

    #[async_trait]
    impl base_proof_contracts::AggregateVerifierClient for RecoveryMockVerifier {
        async fn game_info(
            &self,
            proxy: Address,
        ) -> Result<base_proof_contracts::GameInfo, base_proof_contracts::ContractError> {
            if self.error_proxies.contains(&proxy) {
                return Err(base_proof_contracts::ContractError::Validation(
                    "simulated game_info RPC error".into(),
                ));
            }
            Ok(base_proof_contracts::GameInfo {
                root_claim: self.root_claim,
                l2_block_number: self.l2_block_number,
                parent_index: 0,
            })
        }
        async fn status(&self, _: Address) -> Result<u8, base_proof_contracts::ContractError> {
            Ok(0)
        }
        async fn zk_prover(
            &self,
            _: Address,
        ) -> Result<Address, base_proof_contracts::ContractError> {
            Ok(Address::ZERO)
        }
        async fn tee_prover(
            &self,
            _: Address,
        ) -> Result<Address, base_proof_contracts::ContractError> {
            Ok(Address::ZERO)
        }
        async fn starting_block_number(
            &self,
            _: Address,
        ) -> Result<u64, base_proof_contracts::ContractError> {
            Ok(0)
        }
        async fn read_block_interval(
            &self,
            _: Address,
        ) -> Result<u64, base_proof_contracts::ContractError> {
            Ok(512)
        }
        async fn read_intermediate_block_interval(
            &self,
            _: Address,
        ) -> Result<u64, base_proof_contracts::ContractError> {
            Ok(512)
        }
        async fn intermediate_output_roots(
            &self,
            _: Address,
        ) -> Result<Vec<alloy_primitives::FixedBytes<32>>, base_proof_contracts::ContractError>
        {
            Ok(vec![])
        }
    }

    fn recovery_driver(
        factory: RecoveryMockFactory,
        verifier: RecoveryMockVerifier,
        game_type: u32,
    ) -> Driver<MockL1, MockL2, MockRollupClient, MockAnchorStateRegistry, RecoveryMockFactory>
    {
        let sync_status = test_sync_status(200, B256::ZERO);
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover = test_prover();

        Driver::new(
            DriverConfig { game_type, block_interval: 10, ..Default::default() },
            prover,
            l1,
            l2,
            Arc::new(MockRollupClient { sync_status }),
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) }),
            Arc::new(factory),
            Arc::new(verifier),
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn test_recover_latest_state_no_games_falls_back_to_anchor() {
        let driver = recovery_driver(
            RecoveryMockFactory { games: vec![], error_indices: vec![] },
            RecoveryMockVerifier {
                l2_block_number: 100,
                root_claim: B256::ZERO,
                error_proxies: vec![],
            },
            0,
        );
        let state = driver.recover_latest_state().await.expect("should not error");
        assert_eq!(state.game_index, NO_PARENT_INDEX);
        assert_eq!(state.l2_block_number, 0);
        assert_eq!(state.output_root, B256::ZERO);
    }

    #[tokio::test]
    async fn test_recover_latest_state_finds_matching_type() {
        let root = B256::repeat_byte(0xAA);
        let driver = recovery_driver(
            RecoveryMockFactory { games: vec![(42, Address::ZERO)], error_indices: vec![] },
            RecoveryMockVerifier { l2_block_number: 500, root_claim: root, error_proxies: vec![] },
            42,
        );

        let state = driver.recover_latest_state().await.expect("should not error");
        assert_eq!(state.game_index, 0);
        assert_eq!(state.l2_block_number, 500);
        assert_eq!(state.output_root, root);
    }

    #[tokio::test]
    async fn test_recover_latest_state_no_matching_type_falls_back_to_anchor() {
        let driver = recovery_driver(
            RecoveryMockFactory {
                games: vec![(99, Address::ZERO), (88, Address::ZERO)],
                error_indices: vec![],
            },
            RecoveryMockVerifier {
                l2_block_number: 100,
                root_claim: B256::ZERO,
                error_proxies: vec![],
            },
            42,
        );
        let state = driver.recover_latest_state().await.expect("should not error");
        assert_eq!(state.game_index, NO_PARENT_INDEX);
        assert_eq!(state.l2_block_number, 0);
        assert_eq!(state.output_root, B256::ZERO);
    }

    #[tokio::test]
    async fn test_recover_latest_state_skips_wrong_type() {
        let root = B256::repeat_byte(0xBB);
        let driver = recovery_driver(
            RecoveryMockFactory {
                games: vec![(42, Address::ZERO), (99, Address::ZERO)],
                error_indices: vec![],
            },
            RecoveryMockVerifier { l2_block_number: 200, root_claim: root, error_proxies: vec![] },
            42,
        );

        let state = driver.recover_latest_state().await.expect("should not error");
        assert_eq!(state.game_index, 0);
        assert_eq!(state.l2_block_number, 200);
    }

    #[tokio::test]
    async fn test_recover_latest_state_continues_on_game_at_index_error() {
        let root = B256::repeat_byte(0xCC);
        let driver = recovery_driver(
            RecoveryMockFactory {
                games: vec![(42, Address::ZERO), (42, Address::ZERO)],
                error_indices: vec![1],
            },
            RecoveryMockVerifier { l2_block_number: 300, root_claim: root, error_proxies: vec![] },
            42,
        );

        let state = driver.recover_latest_state().await.expect("should not error");
        assert_eq!(state.game_index, 0);
        assert_eq!(state.l2_block_number, 300);
    }

    #[tokio::test]
    async fn test_recover_latest_state_continues_on_game_info_error() {
        let root = B256::repeat_byte(0xDD);
        let error_proxy = Address::repeat_byte(0xEE);
        let healthy_proxy = Address::repeat_byte(0xFF);
        let driver = recovery_driver(
            RecoveryMockFactory {
                games: vec![(42, healthy_proxy), (42, error_proxy)],
                error_indices: vec![],
            },
            RecoveryMockVerifier {
                l2_block_number: 400,
                root_claim: root,
                error_proxies: vec![error_proxy],
            },
            42,
        );

        let state = driver.recover_latest_state().await.expect("should not error");
        assert_eq!(state.game_index, 0);
        assert_eq!(state.l2_block_number, 400);
        assert_eq!(state.output_root, root);
    }
}
