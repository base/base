//! Recovers proposer onchain state from submitted dispute games.

use std::sync::Arc;

use alloy_primitives::Address;
use base_proof_contracts::{
    AnchorStateRegistryClient, DisputeGameFactoryClient, encode_extra_data,
};
use base_proof_rpc::{RollupProvider, RpcError};
use futures::{StreamExt, TryStreamExt, stream};
use tracing::{debug, info, warn};

use crate::{
    driver::RecoveredState, error::ProposerError, proof_target::ProofTarget,
    proposal_intervals::ProposalIntervals,
};

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
    /// Recovered onchain state from the walk.
    pub state: RecoveredState,
}

/// Recovers the latest submitted proposer state from L1 and rollup RPCs.
pub struct ProofRecovery {
    config: ProofRecoveryConfig,
    rollup_client: Arc<dyn RollupProvider>,
    anchor_registry: Arc<dyn AnchorStateRegistryClient>,
    factory_client: Arc<dyn DisputeGameFactoryClient>,
}

impl std::fmt::Debug for ProofRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofRecovery").field("config", &self.config).finish_non_exhaustive()
    }
}

impl ProofRecovery {
    /// Creates a proposer recovery helper.
    pub fn new(
        config: ProofRecoveryConfig,
        rollup_client: Arc<dyn RollupProvider>,
        anchor_registry: Arc<dyn AnchorStateRegistryClient>,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
    ) -> Self {
        Self { config, rollup_client, anchor_registry, factory_client }
    }

    /// Attempts to recover onchain state and fetch the safe head.
    ///
    /// Returns `None` if either step fails (logged as warnings), allowing the
    /// caller to fall through to the poll-tick sleep.
    pub async fn try_recover_and_plan(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> Option<(RecoveredState, u64)> {
        let sync_status = match self.rollup_client.sync_status().await {
            Ok(status) => status,
            Err(e) => {
                warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                return None;
            }
        };
        let safe_head = if self.config.allow_non_finalized {
            sync_status.safe_l2.number
        } else {
            sync_status.finalized_l2.number
        };

        if let Some(cached) = cache.as_ref() {
            let Some(next_proposal_block) =
                ProofTarget::next_block(cached.state.l2_block_number, self.config.block_interval)
            else {
                warn!(
                    cached_block = cached.state.l2_block_number,
                    block_interval = self.config.block_interval,
                    "Cannot compute next proposal block, skipping recovery"
                );
                return None;
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
        }

        let state = match self.recover_latest_state(cache).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to recover onchain state, retrying next tick");
                return None;
            }
        };

        Some((state, safe_head))
    }

    /// Recovers the latest onchain state using a deterministic forward walk
    /// from the anchor root.
    pub async fn recover_latest_state(
        &self,
        cache: &mut Option<ProofRecoveryCache>,
    ) -> Result<RecoveredState, ProposerError> {
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
        let usable_cache =
            cache.as_ref().filter(|cached| anchor.l2_block_number <= cached.state.l2_block_number);

        if let Some(cached) = usable_cache
            && cached.game_count == count
        {
            debug!(game_count = count, "No changes since last recovery, returning cached state");
            return Ok(cached.state);
        }

        let start = match usable_cache {
            Some(cached) if count > cached.game_count => {
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
    async fn forward_walk(&self, start: &RecoveredState) -> Result<RecoveredState, ProposerError> {
        let mut state = *start;

        while let Some(expected_block) =
            ProofTarget::next_block(state.l2_block_number, self.config.block_interval)
        {
            // Fetch all intermediate roots, including the canonical root for
            // `expected_block`, from the rollup node in one batch.
            let intermediate_blocks = ProposalIntervals::intermediate_block_numbers(
                self.config.block_interval,
                self.config.intermediate_block_interval,
                state.l2_block_number,
            )?;
            let rollup = &self.rollup_client;
            let intermediate_roots = match stream::iter(intermediate_blocks.iter().copied())
                .map(|block_number| async move {
                    rollup
                        .output_at_block(block_number)
                        .await
                        .map(|out| out.output_root)
                        .map_err(ProposerError::Rpc)
                })
                .buffered(self.config.scan_concurrency)
                .try_collect::<Vec<_>>()
                .await
            {
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
                        parent_block = state.l2_block_number,
                        error = %e,
                        "Forward walk failed to fetch canonical roots"
                    );
                    return Err(e);
                }
            };

            let canonical_root = *intermediate_roots.last().ok_or_else(|| {
                ProposerError::Internal(format!(
                    "missing canonical root for expected block {expected_block}"
                ))
            })?;

            let extra_data =
                encode_extra_data(expected_block, state.parent_address, &intermediate_roots);

            let lookup = self
                .factory_client
                .games(self.config.game_type, canonical_root, extra_data)
                .await
                .map_err(|e| {
                    ProposerError::Contract(format!(
                        "games lookup failed at block {expected_block}: {e}"
                    ))
                })?;

            if lookup == Address::ZERO {
                info!(
                    gap_block = expected_block,
                    parent_block = state.l2_block_number,
                    parent_address = %state.parent_address,
                    "No game found at expected block, will propose from here"
                );
                break;
            }

            state = RecoveredState {
                parent_address: lookup,
                output_root: canonical_root,
                l2_block_number: expected_block,
            };
        }

        if state.l2_block_number != start.l2_block_number {
            info!(
                latest_block = state.l2_block_number,
                parent_address = %state.parent_address,
                "Recovery forward walk complete"
            );
        }

        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use alloy_primitives::{Address, B256};
    use base_proof_contracts::{AnchorRoot, encode_extra_data};

    use super::*;
    use crate::test_utils::{
        MockAnchorStateRegistry, MockDisputeGameFactory, MockRollupClient, test_anchor_root,
        test_sync_status,
    };

    const TEST_GAME_TYPE: u32 = 42;
    const TEST_BLOCK_INTERVAL: u64 = 512;
    const TEST_ANCHOR_BLOCK: u64 = 0;

    fn proxy_addr(index: u64) -> Address {
        Address::with_last_byte((index + 1) as u8)
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
        let mut parent = Address::ZERO;
        for i in 0..n {
            let block = anchor_block + block_interval * (i as u64 + 1);
            let root_claim = B256::repeat_byte((i as u8) + 1);
            let parent_block = block - block_interval;
            let intermediate_roots = ProposalIntervals::intermediate_block_numbers(
                block_interval,
                intermediate_block_interval,
                parent_block,
            )
            .unwrap()
            .into_iter()
            .map(|intermediate_block| match intermediate_block {
                n if n == block => root_claim,
                n => B256::repeat_byte(n as u8),
            })
            .collect::<Vec<_>>();
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
            uuid_game_responses: None,
            games_should_fail: false,
            game_count_calls: None,
        };

        (factory, output_roots)
    }

    fn recovery(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
    ) -> ProofRecovery {
        recovery_full(
            factory,
            output_roots,
            test_anchor_root(TEST_ANCHOR_BLOCK),
            Address::ZERO,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            None,
        )
    }

    fn recovery_full(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
        anchor_root: AnchorRoot,
        anchor_game: Address,
        block_interval: u64,
        intermediate_block_interval: u64,
        max_safe_block: Option<u64>,
    ) -> ProofRecovery {
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
            Arc::new(MockAnchorStateRegistry { anchor_root, anchor_game }),
            Arc::new(factory),
        )
    }

    async fn recover_uncached(
        recovery: &ProofRecovery,
    ) -> (RecoveredState, Option<ProofRecoveryCache>) {
        let mut cache: Option<ProofRecoveryCache> = None;
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();
        (state, cache)
    }

    fn cache(
        game_count: u64,
        parent_address: Address,
        output_root: B256,
        l2_block_number: u64,
    ) -> Option<ProofRecoveryCache> {
        Some(ProofRecoveryCache {
            game_count,
            state: RecoveredState { parent_address, output_root, l2_block_number },
        })
    }

    #[tokio::test]
    async fn test_recovery_returns_anchor_when_no_games() {
        let factory = MockDisputeGameFactory::with_games(vec![]);
        let recovery = recovery(factory, HashMap::new());

        let (state, cache) = recover_uncached(&recovery).await;

        assert_eq!(state.parent_address, Address::ZERO, "should return anchor state registry");
        assert_eq!(state.l2_block_number, TEST_ANCHOR_BLOCK, "should return anchor block");
        assert!(cache.is_some(), "cache should still be populated");
    }

    #[tokio::test]
    async fn test_recovery_reads_anchor_root_and_game_from_one_snapshot() {
        let anchor_game = proxy_addr(0);
        let anchor_root = B256::repeat_byte(0xAA);
        let anchor_block = TEST_BLOCK_INTERVAL;
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);

        let recovery = recovery_full(
            factory,
            HashMap::new(),
            AnchorRoot { root: anchor_root, l2_block_number: anchor_block },
            anchor_game,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            None,
        );

        let (state, _) = recover_uncached(&recovery).await;

        assert_eq!(state.parent_address, anchor_game);
        assert_eq!(state.output_root, anchor_root);
        assert_eq!(state.l2_block_number, anchor_block);
    }

    #[tokio::test]
    async fn test_recovery_forward_walk_chain() {
        for (game_count, expected_proxy_index, expected_block) in [
            (1, 0, TEST_BLOCK_INTERVAL),
            (2, 1, TEST_BLOCK_INTERVAL * 2),
            (5, 4, TEST_BLOCK_INTERVAL * 5),
        ] {
            let (factory, output_roots) = game_chain(game_count);
            let recovery = recovery(factory, output_roots);

            let (state, cache) = recover_uncached(&recovery).await;

            assert_eq!(state.parent_address, proxy_addr(expected_proxy_index));
            assert_eq!(state.l2_block_number, expected_block);
            assert_eq!(state.output_root, B256::repeat_byte((expected_proxy_index + 1) as u8));
            assert!(cache.is_some());
        }
    }

    #[tokio::test]
    async fn test_recovery_propagates_games_lookup_failure() {
        let (mut factory, output_roots) = game_chain(2);
        factory.games_should_fail = true;

        let recovery = recovery(factory, output_roots);

        let mut cache: Option<ProofRecoveryCache> = None;
        let result = recovery.recover_latest_state(&mut cache).await;

        assert!(matches!(result, Err(ProposerError::Contract(_))));
    }

    #[tokio::test]
    async fn test_recovery_forward_walk_stops_at_safe_head() {
        let (factory, output_roots) = game_chain(3);

        let recovery = recovery_full(
            factory,
            output_roots,
            test_anchor_root(TEST_ANCHOR_BLOCK),
            Address::ZERO,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            Some(TEST_BLOCK_INTERVAL * 2),
        );

        let (state, _) = recover_uncached(&recovery).await;

        assert_eq!(state.parent_address, proxy_addr(1), "should stop at game 1");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 2);
    }

    #[tokio::test]
    async fn test_recovery_cache_hit_equal_game_count() {
        let (factory, output_roots) = game_chain(1);
        let game_proxy = proxy_addr(0);

        let recovery = recovery(factory, output_roots);

        let (state1, mut cache) = recover_uncached(&recovery).await;
        assert!(cache.is_some(), "cache should be populated after first call");
        assert_eq!(state1.parent_address, game_proxy);
        assert_eq!(state1.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 1);

        let state2 = recovery.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2, state1);
    }

    #[tokio::test]
    async fn test_recovery_cache_incremental_resumes_mid_chain() {
        let (factory, output_roots) = game_chain(5);

        let mut cache = cache(3, proxy_addr(2), B256::repeat_byte(0x03), TEST_BLOCK_INTERVAL * 3);

        let recovery = recovery(factory, output_roots);
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(4), "should reach game 4 from cached tip");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 5);
        assert_eq!(cache.as_ref().unwrap().game_count, 5);
    }

    #[tokio::test]
    async fn test_recovery_cache_incremental_unrelated_games() {
        let (factory, output_roots) = game_chain(1);
        let mut factory_with_extra_count = factory;
        factory_with_extra_count.game_count_override = Some(2);

        let recovery = recovery(factory_with_extra_count, output_roots);

        let mut cache = cache(1, proxy_addr(0), B256::repeat_byte(0x01), TEST_BLOCK_INTERVAL);

        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "should remain at game 0");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 2, "cache updated to new count");
    }

    #[tokio::test]
    async fn test_recovery_cache_invalidated_by_count_decrease() {
        let (factory, output_roots) = game_chain(1);

        let mut cache = cache(5, proxy_addr(99), B256::repeat_byte(0xDD), 5 * TEST_BLOCK_INTERVAL);

        let recovery = recovery(factory, output_roots);
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "reorg: should find the 1 remaining game");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 1, "reorg: cache should reflect new count");
    }

    #[tokio::test]
    async fn test_recovery_cache_full_walk_when_anchor_past_tip() {
        let anchor_block = TEST_BLOCK_INTERVAL * 4;
        let (factory, output_roots) =
            game_chain_full(1, anchor_block, TEST_BLOCK_INTERVAL, TEST_BLOCK_INTERVAL);

        let mut cache = cache(0, proxy_addr(99), B256::repeat_byte(0xDD), TEST_BLOCK_INTERVAL);

        let recovery = recovery_full(
            factory,
            output_roots,
            test_anchor_root(anchor_block),
            Address::ZERO,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
            None,
        );
        let state = recovery.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, anchor_block + TEST_BLOCK_INTERVAL);
    }

    #[tokio::test]
    async fn test_recovery_forward_walk_with_intermediate_roots() {
        const RECOVERY_BI: u64 = 4;
        const RECOVERY_IBI: u64 = 2;

        let (factory, output_roots) =
            game_chain_full(2, TEST_ANCHOR_BLOCK, RECOVERY_BI, RECOVERY_IBI);

        let recovery = recovery_full(
            factory,
            output_roots,
            test_anchor_root(TEST_ANCHOR_BLOCK),
            Address::ZERO,
            RECOVERY_BI,
            RECOVERY_IBI,
            None,
        );

        let (state, _) = recover_uncached(&recovery).await;

        assert_eq!(state.parent_address, proxy_addr(1));
        assert_eq!(state.l2_block_number, RECOVERY_BI * 2);
    }
}
