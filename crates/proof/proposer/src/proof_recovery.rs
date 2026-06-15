//! Recovers proposer on-chain state from submitted dispute games.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_proof_contracts::{
    AnchorStateRegistryClient, DisputeGameFactoryClient, encode_extra_data,
};
use base_proof_rpc::{RollupProvider, RpcError};
use futures::{StreamExt, stream};
use tracing::{debug, info, warn};

use crate::{driver::RecoveredState, error::ProposerError, proposal_intervals::ProposalIntervals};

/// Runtime settings for proposer recovery.
#[derive(Debug, Clone, Copy)]
pub struct ProofRecoveryConfig {
    /// Number of L2 blocks between output proposals.
    pub block_interval: u64,
    /// Number of L2 blocks between intermediate output roots.
    pub intermediate_block_interval: u64,
    /// Dispute game type used for proposals.
    pub game_type: u32,
    /// Whether recovery may use the safe head rather than finalized head.
    pub allow_non_finalized: bool,
    /// Address used as the parent sentinel when the anchor has no game.
    pub anchor_state_registry_address: Address,
    /// Maximum number of concurrent rollup RPC calls during root fetching.
    pub scan_concurrency: usize,
}

/// Cached result from the last successful recovery walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofRecoveryCache {
    /// Factory `game_count` at the time of the walk.
    pub game_count: u64,
    /// Recovered on-chain state from the walk.
    pub state: RecoveredState,
}

/// Recovery hook used by collector orchestration after successful submissions.
#[async_trait]
pub trait ProofCollectorRecoveryProvider: Send + Sync {
    /// Refreshes the recovery cache and returns the latest on-chain state.
    async fn recover_latest_state(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> std::result::Result<RecoveredState, ProposerError>;
}

/// Recovers the latest submitted proposer state from L1 and rollup RPCs.
pub struct ProofRecovery<R, ASR, F>
where
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    config: ProofRecoveryConfig,
    rollup_client: Arc<R>,
    anchor_registry: Arc<ASR>,
    factory_client: Arc<F>,
}

impl<R, ASR, F> std::fmt::Debug for ProofRecovery<R, ASR, F>
where
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofRecovery").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<R, ASR, F> Clone for ProofRecovery<R, ASR, F>
where
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config,
            rollup_client: Arc::clone(&self.rollup_client),
            anchor_registry: Arc::clone(&self.anchor_registry),
            factory_client: Arc::clone(&self.factory_client),
        }
    }
}

impl<R, ASR, F> ProofRecovery<R, ASR, F>
where
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    /// Creates a proposer recovery helper.
    pub const fn new(
        config: ProofRecoveryConfig,
        rollup_client: Arc<R>,
        anchor_registry: Arc<ASR>,
        factory_client: Arc<F>,
    ) -> Self {
        Self { config, rollup_client, anchor_registry, factory_client }
    }

    /// Attempts to recover on-chain state and fetch the safe head.
    ///
    /// Returns `None` if either step fails (logged as warnings), allowing the
    /// caller to fall through to the poll-tick sleep.
    pub async fn try_recover_and_plan(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> Option<(RecoveredState, u64)> {
        if let Some(cached) = cache.as_ref() {
            let safe_head = match self.latest_safe_block_number().await {
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                    return None;
                }
            };

            let next_proposal_block =
                match cached.state.l2_block_number.checked_add(self.config.block_interval) {
                    Some(block) => block,
                    None => {
                        warn!(
                            cached_block = cached.state.l2_block_number,
                            block_interval = self.config.block_interval,
                            "Cannot compute next proposal block, retrying next tick"
                        );
                        return None;
                    }
                };

            if safe_head < next_proposal_block {
                debug!(
                    safe_head,
                    cached_block = cached.state.l2_block_number,
                    next_proposal_block,
                    "Safe head below next proposal target, skipping recovery"
                );
                return Some((cached.state, safe_head));
            }

            let state = match self.recover_latest_state(cache).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to recover on-chain state, retrying next tick");
                    return None;
                }
            };

            return Some((state, safe_head));
        }

        let (state_result, safe_head_result) =
            tokio::join!(self.recover_latest_state(cache), self.latest_safe_block_number(),);

        let state = match state_result {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to recover on-chain state, retrying next tick");
                return None;
            }
        };

        let safe_head = match safe_head_result {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                return None;
            }
        };

        Some((state, safe_head))
    }

    /// Recovers the latest on-chain state using a deterministic forward walk
    /// from the anchor root.
    ///
    /// # Strategy
    ///
    /// 1. Read `game_count` from the factory and anchor root from the registry
    ///    once the safe head is high enough to need recovery.
    /// 2. Cache check. If both `game_count` and `anchor_root` match the cache,
    ///    return the cached state immediately.
    /// 3. Forward walk. Walk from the anchor block, stepping by
    ///    `block_interval`, and use UUID-based `games()` lookups to find the
    ///    latest submitted game.
    async fn recover_latest_state(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> std::result::Result<RecoveredState, ProposerError> {
        let count = self
            .factory_client
            .game_count()
            .await
            .map_err(|e| ProposerError::Contract(format!("recovery game_count failed: {e}")))?;

        // Read the anchor root and anchor game from one L1 snapshot so
        // recovery cannot combine an old root with a newer anchor game.
        let anchor_snapshot = self
            .anchor_registry
            .anchor_snapshot()
            .await
            .map_err(|e| ProposerError::Contract(format!("anchor_snapshot failed: {e}")))?;
        let anchor = anchor_snapshot.anchor_root;

        // The cached tip is valid as long as the anchor hasn't advanced past
        // it. The anchor advances when games resolve, but it always stays
        // behind the chain tip.
        let tip_still_valid =
            |cached: &ProofRecoveryCache| anchor.l2_block_number <= cached.state.l2_block_number;

        if let Some(cached) = cache.as_ref()
            && tip_still_valid(cached)
            && cached.game_count == count
        {
            debug!(game_count = count, "No changes since last recovery, returning cached state");
            return Ok(cached.state);
        }

        let start = match cache.as_ref() {
            Some(cached) if tip_still_valid(cached) && count > cached.game_count => {
                debug!(
                    cached_block = cached.state.l2_block_number,
                    old_count = cached.game_count,
                    new_count = count,
                    "Resuming forward walk from cached tip"
                );
                cached.state
            }
            _ => {
                let parent_address = if anchor_snapshot.anchor_game.is_zero() {
                    self.config.anchor_state_registry_address
                } else {
                    anchor_snapshot.anchor_game
                };

                RecoveredState {
                    parent_address,
                    output_root: anchor.root,
                    l2_block_number: anchor.l2_block_number,
                }
            }
        };

        let state = self.forward_walk(&start).await?;

        *cache = Some(ProofRecoveryCache { game_count: count, state });
        Ok(state)
    }

    /// Performs a deterministic forward walk to find the latest verified game
    /// using UUID-based `games()` lookups.
    async fn forward_walk(
        &self,
        start: &RecoveredState,
    ) -> std::result::Result<RecoveredState, ProposerError> {
        let block_interval = self.config.block_interval;
        let game_type = self.config.game_type;

        let log_interval = (block_interval / 5).max(1);

        let mut parent_address = start.parent_address;
        let mut parent_output_root = start.output_root;
        let mut parent_block = start.l2_block_number;
        let mut steps: u64 = 0;

        while let Some(expected_block) = parent_block.checked_add(block_interval) {
            // Fetch all intermediate roots, including the canonical root for
            // `expected_block`, from the rollup node in one batch.
            let intermediate_blocks = ProposalIntervals::intermediate_block_numbers(
                self.config.block_interval,
                self.config.intermediate_block_interval,
                parent_block,
            )?;
            let intermediate_roots =
                match self.fetch_canonical_roots(intermediate_blocks.clone()).await {
                    Ok(roots) => roots,
                    Err(ProposerError::Rpc(RpcError::BlockNotFound(_))) => {
                        debug!(
                            block = expected_block,
                            "Block not available yet, treating as end of walk"
                        );
                        break;
                    }
                    Err(e) => {
                        warn!(
                            expected_block,
                            parent_block,
                            error = %e,
                            "Forward walk failed to fetch canonical roots"
                        );
                        return Err(e);
                    }
                };

            let canonical_root = *intermediate_roots.get(&expected_block).ok_or_else(|| {
                ProposerError::Internal(format!(
                    "missing canonical root for expected block {expected_block}"
                ))
            })?;

            let intermediate_root_vec: Vec<B256> = intermediate_blocks
                .iter()
                .map(|ib| {
                    intermediate_roots.get(ib).copied().ok_or_else(|| {
                        ProposerError::Internal(format!(
                            "missing canonical root for intermediate block {ib}"
                        ))
                    })
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let extra_data =
                encode_extra_data(expected_block, parent_address, &intermediate_root_vec);

            let lookup =
                self.factory_client.games(game_type, canonical_root, extra_data).await.map_err(
                    |e| {
                        ProposerError::Contract(format!(
                            "games lookup failed at block {expected_block}: {e}"
                        ))
                    },
                )?;

            if lookup == Address::ZERO {
                info!(
                    gap_block = expected_block,
                    parent_block,
                    parent_address = %parent_address,
                    games_verified = steps,
                    "No game found at expected block, will propose from here"
                );
                break;
            }

            parent_address = lookup;
            parent_output_root = canonical_root;
            parent_block = expected_block;
            steps += 1;

            if steps.is_multiple_of(log_interval) {
                info!(
                    games_verified = steps,
                    latest_block = parent_block,
                    "Recovery forward walk in progress"
                );
            }
        }

        if steps > 0 {
            info!(
                latest_block = parent_block,
                parent_address = %parent_address,
                games_verified = steps,
                "Recovery forward walk complete"
            );
        }

        Ok(RecoveredState {
            parent_address,
            output_root: parent_output_root,
            l2_block_number: parent_block,
        })
    }

    /// Returns the latest safe L2 block number used for recovery planning.
    async fn latest_safe_block_number(&self) -> std::result::Result<u64, ProposerError> {
        let sync_status = self.rollup_client.sync_status().await?;
        if self.config.allow_non_finalized {
            Ok(sync_status.safe_l2.number)
        } else {
            Ok(sync_status.finalized_l2.number)
        }
    }

    /// Concurrently fetches canonical output roots for the given block numbers.
    async fn fetch_canonical_roots(
        &self,
        blocks: Vec<u64>,
    ) -> std::result::Result<HashMap<u64, B256>, ProposerError> {
        self.fetch_canonical_root_results(blocks)
            .await
            .into_iter()
            .map(|(block_number, result)| result.map(|root| (block_number, root)))
            .collect()
    }

    async fn fetch_canonical_root_results(
        &self,
        blocks: Vec<u64>,
    ) -> HashMap<u64, std::result::Result<B256, ProposerError>> {
        if blocks.is_empty() {
            return HashMap::new();
        }
        stream::iter(blocks)
            .map(|block_number| {
                let rollup = &self.rollup_client;
                async move {
                    let result = rollup
                        .output_at_block(block_number)
                        .await
                        .map(|out| out.output_root)
                        .map_err(ProposerError::Rpc);
                    (block_number, result)
                }
            })
            .buffered(self.config.scan_concurrency)
            .collect()
            .await
    }
}

#[async_trait]
impl<R, ASR, F> ProofCollectorRecoveryProvider for ProofRecovery<R, ASR, F>
where
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    async fn recover_latest_state(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> std::result::Result<RecoveredState, ProposerError> {
        Self::recover_latest_state(self, cache).await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_contracts::{
        AnchorSnapshot, AnchorStateRegistryClient, ContractError, encode_extra_data,
    };
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{
        MockAnchorStateRegistry, MockDisputeGameFactory, MockRollupClient, test_anchor_root,
        test_sync_status,
    };

    const TEST_GAME_TYPE: u32 = 42;
    const TEST_BLOCK_INTERVAL: u64 = 512;
    const TEST_ANCHOR_BLOCK: u64 = 0;

    type TestRecovery =
        ProofRecovery<MockRollupClient, MockAnchorStateRegistry, MockDisputeGameFactory>;

    #[derive(Debug)]
    struct SnapshotOnlyAnchorStateRegistry {
        snapshot: AnchorSnapshot,
    }

    #[async_trait]
    impl AnchorStateRegistryClient for SnapshotOnlyAnchorStateRegistry {
        async fn anchor_snapshot(&self) -> std::result::Result<AnchorSnapshot, ContractError> {
            Ok(self.snapshot)
        }
    }

    fn proxy_addr(index: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..20].copy_from_slice(&(index + 1).to_be_bytes());
        Address::new(bytes)
    }

    fn game_chain(n: usize) -> (MockDisputeGameFactory, HashMap<u64, B256>) {
        game_chain_full(n, TEST_ANCHOR_BLOCK, TEST_BLOCK_INTERVAL, TEST_BLOCK_INTERVAL)
    }

    fn game_chain_full(
        n: usize,
        anchor_block: u64,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> (MockDisputeGameFactory, HashMap<u64, B256>) {
        let mut uuid_games = HashMap::new();
        let mut output_roots = HashMap::new();
        let intermediate_count = block_interval / intermediate_block_interval;

        let mut parent = Address::ZERO;
        for i in 0..n {
            let block = anchor_block + block_interval * (i as u64 + 1);
            let root_claim = B256::repeat_byte((i as u8) + 1);
            let parent_block = block - block_interval;
            let mut intermediate_roots = Vec::with_capacity(intermediate_count as usize);
            for j in 1..=intermediate_count {
                let intermediate_block = parent_block + j * intermediate_block_interval;
                let intermediate_root = if intermediate_block == block {
                    root_claim
                } else {
                    B256::repeat_byte(intermediate_block as u8)
                };
                output_roots.insert(intermediate_block, intermediate_root);
                intermediate_roots.push(intermediate_root);
            }
            output_roots.insert(block, root_claim);

            let extra_data = encode_extra_data(block, parent, &intermediate_roots);
            let proxy = proxy_addr(i as u64);

            uuid_games.insert((TEST_GAME_TYPE, root_claim, extra_data), proxy);

            parent = proxy;
        }

        let factory = MockDisputeGameFactory {
            games: Vec::new(),
            game_count_override: Some(n as u64),
            uuid_games,
            games_should_fail: false,
            game_count_calls: None,
        };

        (factory, output_roots)
    }

    fn recovery(factory: MockDisputeGameFactory, output_roots: HashMap<u64, B256>) -> TestRecovery {
        recovery_full(
            factory,
            output_roots,
            TEST_ANCHOR_BLOCK,
            Address::ZERO,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            None,
        )
    }

    fn recovery_full(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
        anchor_block: u64,
        anchor_game: Address,
        block_interval: u64,
        intermediate_block_interval: u64,
        max_safe_block: Option<u64>,
    ) -> TestRecovery {
        ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval,
                intermediate_block_interval,
                game_type: TEST_GAME_TYPE,
                allow_non_finalized: false,
                anchor_state_registry_address: Address::ZERO,
                scan_concurrency: 8,
            },
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(0, B256::ZERO),
                output_roots,
                max_safe_block,
            }),
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(anchor_block),
                anchor_game,
            }),
            Arc::new(factory),
        )
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_returns_anchor_when_no_games() {
        let factory = MockDisputeGameFactory::with_games(vec![]);
        let recovery = recovery(factory, HashMap::new());

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, Address::ZERO, "should return anchor state registry");
        assert_eq!(state.l2_block_number, TEST_ANCHOR_BLOCK, "should return anchor block");
        assert!(cache.is_some(), "cache should still be populated");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cold_start_uses_anchor_game_after_anchor_advance() {
        let anchor_game = proxy_addr(0);
        let anchor_block = TEST_BLOCK_INTERVAL;

        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);
        let recovery = recovery_full(
            factory,
            HashMap::new(),
            anchor_block,
            anchor_game,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            None,
        );

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, anchor_game, "advanced anchor game should be the parent");
        assert_eq!(state.l2_block_number, anchor_block, "should propose after the live anchor");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_reads_anchor_root_and_game_from_one_snapshot() {
        let anchor_game = proxy_addr(0);
        let anchor_root = B256::repeat_byte(0xAA);
        let anchor_block = TEST_BLOCK_INTERVAL;
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);

        let recovery = ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval: TEST_BLOCK_INTERVAL,
                intermediate_block_interval: TEST_BLOCK_INTERVAL,
                game_type: TEST_GAME_TYPE,
                allow_non_finalized: false,
                anchor_state_registry_address: Address::ZERO,
                scan_concurrency: 8,
            },
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(TEST_BLOCK_INTERVAL * 2, B256::ZERO),
                output_roots: HashMap::new(),
                max_safe_block: None,
            }),
            Arc::new(SnapshotOnlyAnchorStateRegistry {
                snapshot: AnchorSnapshot {
                    anchor_root: base_proof_contracts::AnchorRoot {
                        root: anchor_root,
                        l2_block_number: anchor_block,
                    },
                    anchor_game,
                },
            }),
            Arc::new(factory),
        );

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, anchor_game);
        assert_eq!(state.output_root, anchor_root);
        assert_eq!(state.l2_block_number, anchor_block);
    }

    #[rstest]
    #[case::single_game(1, 0, TEST_BLOCK_INTERVAL, "single game at first interval")]
    #[case::chain_of_two(2, 1, TEST_BLOCK_INTERVAL * 2, "chain of two sequential games")]
    #[case::chain_of_five(5, 4, TEST_BLOCK_INTERVAL * 5, "chain of five sequential games")]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_chain(
        #[case] game_count: usize,
        #[case] expected_proxy_index: u64,
        #[case] expected_block: u64,
        #[case] scenario: &str,
    ) {
        let (factory, output_roots) = game_chain(game_count);
        let recovery = recovery(factory, output_roots);

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(expected_proxy_index), "{scenario}");
        assert_eq!(state.l2_block_number, expected_block, "{scenario}");
        assert!(cache.is_some(), "{scenario}: cache should be populated");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_gap() {
        let root_1 = B256::repeat_byte(0x01);
        let extra_data_1 = encode_extra_data(TEST_BLOCK_INTERVAL, Address::ZERO, &[root_1]);

        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);
        factory.uuid_games.insert((TEST_GAME_TYPE, root_1, extra_data_1), proxy_addr(0));

        let output_roots = HashMap::from([(TEST_BLOCK_INTERVAL, root_1)]);
        let recovery = recovery(factory, output_roots);

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "should stop at first game before gap");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(state.output_root, root_1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_propagates_games_lookup_failure() {
        let (mut factory, output_roots) = game_chain(2);
        factory.games_should_fail = true;

        let recovery = recovery(factory, output_roots);

        let mut cache: Option<ProofRecoveryCache> = None;
        let result = recovery.recover_latest_state(&mut cache).await;

        assert!(result.is_err(), "games() failure should propagate");
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProposerError::Contract(_)),
            "expected ProposerError::Contract, got {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_safe_head() {
        let (factory, output_roots) = game_chain(3);

        let recovery = recovery_full(
            factory,
            output_roots,
            TEST_ANCHOR_BLOCK,
            Address::ZERO,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            Some(TEST_BLOCK_INTERVAL * 2),
        );

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(1), "should stop at game 1");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 2);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_hit_equal_game_count() {
        let (factory, output_roots) = game_chain(1);
        let game_proxy = proxy_addr(0);

        let recovery = recovery(factory, output_roots);

        let mut cache: Option<ProofRecoveryCache> = None;
        let state1 = recovery.recover_latest_state(&mut cache).await.unwrap();
        assert!(cache.is_some(), "cache should be populated after first call");
        assert_eq!(state1.parent_address, game_proxy);
        assert_eq!(state1.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 1);

        let state2 = recovery.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.parent_address, state1.parent_address);
        assert_eq!(state2.l2_block_number, state1.l2_block_number);
        assert_eq!(state2.output_root, state1.output_root);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_on_count_increase() {
        let (factory, output_roots) = game_chain(2);

        let mut cache = Some(ProofRecoveryCache {
            game_count: 1,
            state: RecoveredState {
                parent_address: proxy_addr(0),
                output_root: B256::repeat_byte(0x01),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let recovery = recovery(factory, output_roots);
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(1), "should find game 1 incrementally");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 2);
        assert_eq!(cache.as_ref().unwrap().game_count, 2, "cache should reflect new count");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_resumes_mid_chain() {
        let (factory, output_roots) = game_chain(5);

        let mut cache = Some(ProofRecoveryCache {
            game_count: 3,
            state: RecoveredState {
                parent_address: proxy_addr(2),
                output_root: B256::repeat_byte(0x03),
                l2_block_number: TEST_BLOCK_INTERVAL * 3,
            },
        });

        let recovery = recovery(factory, output_roots);
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(4), "should reach game 4 from cached tip");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 5);
        assert_eq!(cache.as_ref().unwrap().game_count, 5);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_unrelated_games() {
        let (factory, output_roots) = game_chain(1);
        let mut factory_with_extra_count = factory;
        factory_with_extra_count.game_count_override = Some(2);

        let recovery = recovery(factory_with_extra_count, output_roots);

        let mut cache = Some(ProofRecoveryCache {
            game_count: 1,
            state: RecoveredState {
                parent_address: proxy_addr(0),
                output_root: B256::repeat_byte(0x01),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "should remain at game 0");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 2, "cache updated to new count");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_invalidated_by_count_decrease() {
        let (factory, output_roots) = game_chain(1);

        let mut cache = Some(ProofRecoveryCache {
            game_count: 5,
            state: RecoveredState {
                parent_address: proxy_addr(99),
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: 5 * TEST_BLOCK_INTERVAL,
            },
        });

        let recovery = recovery(factory, output_roots);
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "reorg: should find the 1 remaining game");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 1, "reorg: cache should reflect new count");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_full_walk_when_anchor_past_tip() {
        let anchor_block = TEST_BLOCK_INTERVAL * 4;
        let (factory, output_roots) =
            game_chain_full(1, anchor_block, TEST_BLOCK_INTERVAL, TEST_BLOCK_INTERVAL);

        let mut cache = Some(ProofRecoveryCache {
            game_count: 0,
            state: RecoveredState {
                parent_address: proxy_addr(99),
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let recovery = recovery_full(
            factory,
            output_roots,
            anchor_block,
            Address::ZERO,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            None,
        );
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, anchor_block + TEST_BLOCK_INTERVAL);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_with_intermediate_roots() {
        const RECOVERY_BI: u64 = 4;
        const RECOVERY_IBI: u64 = 2;

        let (factory, output_roots) =
            game_chain_full(2, TEST_ANCHOR_BLOCK, RECOVERY_BI, RECOVERY_IBI);

        let recovery = recovery_full(
            factory,
            output_roots,
            TEST_ANCHOR_BLOCK,
            Address::ZERO,
            RECOVERY_BI,
            RECOVERY_IBI,
            None,
        );

        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(1));
        assert_eq!(state.l2_block_number, RECOVERY_BI * 2);
    }
}
