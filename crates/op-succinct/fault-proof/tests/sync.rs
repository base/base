pub mod common;

#[cfg(feature = "integration")]
mod sync {
    use std::collections::HashMap;

    use crate::common::{
        constants::{
            DISPUTE_GAME_FINALITY_DELAY_SECONDS, MAX_CHALLENGE_DURATION, MAX_PROVE_DURATION,
            MOCK_PERMISSIONED_GAME_TYPE, PROPOSER_ADDRESS, TEST_GAME_TYPE,
        },
        TestEnvironment,
    };
    use alloy_primitives::{Bytes, FixedBytes, Uint, U256};
    use alloy_sol_types::{SolCall, SolValue};
    use anyhow::{Context, Result};
    use fault_proof::{
        contract::ProposalStatus,
        proposer::{
            Game, GameFetchResult, OPSuccinctProposer, ProposerStateSnapshot, MAX_GAME_DEADLINE_LAG,
        },
    };
    use op_succinct_bindings::dispute_game_factory::DisputeGameFactory;
    use op_succinct_host_utils::host::OPSuccinctHost;
    use rand::Rng;
    use rstest::rstest;

    const M: u32 = u32::MAX;

    async fn setup() -> Result<(
        TestEnvironment,
        OPSuccinctProposer<fault_proof::L1Provider, impl OPSuccinctHost + Clone>,
        Uint<256, 4>,
    )> {
        let env = TestEnvironment::setup().await?;
        let factory = env.factory()?;
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;
        let proposer = env.init_proposer().await?;
        Ok((env, proposer, init_bond))
    }

    async fn cached_game<H: OPSuccinctHost + Clone>(
        proposer: &OPSuccinctProposer<fault_proof::L1Provider, H>,
        index: u64,
    ) -> Result<Game> {
        proposer
            .get_game(U256::from(index))
            .await
            .with_context(|| format!("game {index} missing from cache"))
    }

    trait Assertion {
        fn assert_game_len(&self, expected_len: usize);
        fn assert_anchor_index(&self, expected_index: Option<usize>);
        fn assert_canonical_head(
            &self,
            expected_canonical_head_index: Option<u64>,
            expected_processed_l2_block: u64,
            starting_l2_block: u64,
        );
    }

    impl Assertion for ProposerStateSnapshot {
        fn assert_game_len(&self, expected_len: usize) {
            assert_eq!(self.games.len(), expected_len, "Number of synced games should match");
        }

        fn assert_anchor_index(&self, expected_index: Option<usize>) {
            assert_eq!(
                self.anchor_index,
                expected_index.map(U256::from),
                "Anchor index should match"
            );
        }

        fn assert_canonical_head(
            &self,
            expected_canonical_head_index: Option<u64>,
            expected_processed_l2_block: u64,
            starting_l2_block: u64,
        ) {
            let expected_index_u256 = expected_canonical_head_index.map(U256::from);
            assert_eq!(
                self.canonical_head_index, expected_index_u256,
                "Canonical head index should match"
            );
            let expected_l2_block = expected_processed_l2_block + starting_l2_block;
            assert_eq!(
                self.canonical_head_l2_block,
                U256::from(expected_l2_block),
                "Canonical head L2 block should match"
            );
        }
    }

    /// Verifies proposer state sync across multiple happy-path scenarios with only valid games.
    /// Exercises different branching structures, parent-child relationships, and per-game L2 block
    /// intervals.
    ///
    /// Case naming guide:
    /// - "noanch_*": anchored game absent.
    /// - "anch_*": anchored game present.
    #[rstest]
    #[case::zero_games(0, &[], &[M], &[], None, 0)]
    #[case::noanch_single_game_default_interval(1, &[], &[M], &[], Some(0), 1)] // Branch: M->0, Blocks: 0->1
    #[case::noanch_single_game_large_interval(1, &[], &[M], &[10], Some(0), 10)] // Branch: M->0, Blocks: 0->10
    #[case::noanch_two_games_same_branch(2, &[], &[M, 0], &[], Some(1), 2)] // Branch: M->0->1, Blocks: 0->1->2
    #[case::noanch_two_games_same_parent_diff_intervals(2, &[], &[M, M], &[1, 2], Some(1), 2)] // Branches: M->0, M->1, Blocks: 0->1, 0->2
    #[case::noanch_three_games_same_branch(3,&[], &[M, 0, 1], &[], Some(2), 3)] // Branch: M->0->1->2, Blocks: 0->1->2->3
    #[case::noanch_three_games_same_parent_diff_intervals_1(3,&[], &[M, M, M], &[1, 2, 3], Some(2), 3)] // Branches: M->0, M->1, M->2, Blocks: 0->1, 0->2, 0->3
    #[case::noanch_three_games_same_parent_diff_intervals_2(3,&[], &[M, M, M], &[1, 3, 2], Some(1), 3)] // Branches: M->0, M->1, M->2, Blocks: 0->1, 0->3, 0->2
    #[case::noanch_three_games_same_branch_diff_intervals_1(3,&[], &[M, 0, 1], &[1, 2, 3], Some(2), 6)] // Branch: M->0->1->2, Blocks: 0->1->3->6
    #[case::noanch_three_games_same_branch_diff_intervals_2(3, &[], &[M, 0, 1], &[1, 3, 2], Some(2), 6)] // Branch: M->0->1->2, Blocks: 0->1->4->6
    #[case::noanch_three_games_two_branches_diff_intervals(3,&[], &[M, 0, M], &[1, 3, 2], Some(1), 4)] // Branches: M->0->1, M->2, Blocks: 0->1->4 and 0->2
    #[case::noanch_five_games_two_branches(5, &[], &[M, 0, 1, 0, 3], &[1, 1, 1, 2, 2], Some(4), 5)] // Branches: M->0->1->2, 0->3->4, Blocks: 0->1->2->3 and 1->3->5
    #[case::noanch_five_games_three_branches(5, &[], &[M, 0, 1, 0, 0], &[1, 1, 1, 4, 3], Some(3), 5)] // Branches: M->0->1->2, 0->3, 0->4, Blocks: 0->1->2->3, 1->5, 1->4
    #[case::anch_single_game_default_interval(1, &[0], &[M], &[], Some(0), 1)]
    #[case::anch_two_games_same_branch(2, &[0, 1], &[M, 0], &[], Some(1), 2)]
    #[case::anch_two_games_genesis_parent_override(2, &[0], &[M, M], &[1, 2], Some(1), 2)] // M->0(anchor), M->1, game 1 overrides (genesis, higher block)
    #[case::anch_lower_parent_override(4, &[0, 1], &[M, 0, 1, 0], &[1, 1, 1, 3], Some(3), 4)] // M->0->1(anchor)->2, 0->3, game 3 overrides (lower parent, higher block)
    #[case::anch_no_override_lower_block(4, &[0, 1], &[M, 0, 1, 0], &[1, 1, 2, 2], Some(2), 4)] // M->0->1(anchor)->2, 0->3, no override (game 3 block < game 2)
    #[case::anch_five_games_two_branches(5, &[0, 1], &[M, 0, 1, 0, 3], &[1, 1, 1, 2, 2], Some(2), 3)]
    #[tokio::test]
    async fn test_sync_state_happy_paths(
        #[case] num_games: usize,
        #[case] anchor_ids: &[usize],
        #[case] parent_ids: &[u32],
        #[case] intervals: &[u64],
        #[case] expected_canonical_head_index: Option<u64>,
        #[case] expected_processed_l2_block: u64,
    ) -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let mut starting_blocks: HashMap<u32, u64> = HashMap::new();

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut block = starting_l2_block;
        for (i, _) in parent_ids.iter().take(num_games).enumerate() {
            let cur_parent_id = parent_ids[i];
            starting_blocks.insert(cur_parent_id, block);

            let end_block = block + intervals.get(i).unwrap_or(&1);
            let root_claim = env.compute_output_root_at_block(end_block).await?;
            env.create_game(root_claim, end_block, cur_parent_id, init_bond).await?;
            let (index, address) = env.last_game_info().await?;
            tracing::info!("✓ Created game {index} with parent {cur_parent_id}");

            if anchor_ids.contains(&i) {
                env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
                env.resolve_game(address).await?;
                env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
                env.set_anchor_state(address).await?;
                tracing::info!("Anchor game set to index {index}");
            }

            // Determine the starting block for the next game
            //
            // If the next game's parent is the current game, the next game's starting block
            // is the end block of the current game.
            // Otherwise, look up the starting block from the map.
            let next_parent_id = parent_ids.get(i + 1).copied().unwrap_or(M);
            if cur_parent_id.wrapping_add(1) == next_parent_id {
                block = end_block;
            } else {
                block = *starting_blocks.get(&next_parent_id).unwrap_or(&end_block);
            }
        }

        proposer.sync_state().await?;

        let snapshot = proposer.state_snapshot().await;

        let expected_anchor_index = anchor_ids.iter().max().copied();

        snapshot.assert_game_len(num_games);
        snapshot.assert_anchor_index(expected_anchor_index);
        snapshot.assert_canonical_head(
            expected_canonical_head_index,
            expected_processed_l2_block,
            starting_l2_block,
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_state_with_game_already_exists() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let mut parent_id = M;
        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut block = starting_l2_block;
        for _ in 0..10 {
            block += 1;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            parent_id = if parent_id == M { 0 } else { parent_id + 1 };
        }

        proposer.sync_state().await?;

        for i in 0..10 {
            let fetch_result = proposer.fetch_game(U256::from(i)).await?;
            assert!(matches!(fetch_result, GameFetchResult::AlreadyExists));
        }

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(10);
        snapshot.assert_anchor_index(None);
        snapshot.assert_canonical_head(Some(9), 10, starting_l2_block);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_state_drops_invalid_claim() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let block = starting_l2_block + 1;
        let mut rng = rand::rng();
        let mut invalid_root_bytes = [0u8; 32];
        rng.fill(&mut invalid_root_bytes);
        let invalid_root = FixedBytes::<32>::from(invalid_root_bytes);
        env.create_game(invalid_root, block, M, init_bond).await?;

        proposer.sync_state().await?;

        let fetch_result = proposer.fetch_game(U256::from(0)).await?;
        assert!(matches!(fetch_result, GameFetchResult::InvalidGame { .. }));

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(0);
        snapshot.assert_anchor_index(None);
        snapshot.assert_canonical_head(None, 0, starting_l2_block);

        Ok(())
    }

    /// Verifies sync prunes invalid game branches while preserving the valid subtree, and sets the
    /// correct game count, anchor index, and canonical head.
    ///
    /// The test creates 10 games with the following parent-child relationships:
    /// - Branch 1: M -> 0 -> 1 -> 2 -> 3 -> 4 -> 5
    /// - Branch 2: 3 -> 6 -> 7 -> 8 -> 9
    #[rstest]
    #[case::anchor_none_invalid_4_5(None, &[4, 5], 8, Some(9), 10)]
    #[case::anchor_0_invalid_4_5(Some(0), &[4, 5], 8, Some(9), 10)]
    #[case::anchor_3_invalid_4_5(Some(3), &[4, 5], 8, Some(9), 10)]
    #[case::anchor_9_invalid_4_5(Some(9), &[4, 5], 8, Some(9), 10)]
    #[case::anchor_0_invalid_6(Some(0), &[6], 6, Some(5), 6)]
    #[case::anchor_5_invalid_6(Some(5), &[6], 6, Some(5), 6)]
    #[case::anchor_none_invalid_3(None, &[3], 3, Some(2), 3)]
    #[case::anchor_0_invalid_3(Some(0), &[3], 3, Some(2), 3)]
    #[tokio::test]
    async fn test_sync_state_with_invalid_games_removes_subtree(
        #[case] anchor_index: Option<usize>,
        #[case] invalid_game_ids: &[usize],
        #[case] expected_games_len: usize,
        #[case] expected_canonical_head_index: Option<u64>,
        #[case] expected_processed_l2_block: u64,
    ) -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let parent_ids = [M, 0, 1, 2, 3, 4, 3, 6, 7, 8];
        let mut game_addrs = Vec::with_capacity(10);

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut block = starting_l2_block;

        // Game creation step
        for (i, parent_id) in parent_ids.into_iter().enumerate() {
            block += 1;

            let root_claim = if invalid_game_ids.contains(&i) {
                let mut rng = rand::rng();
                let mut invalid_root_bytes = [0u8; 32];
                rng.fill(&mut invalid_root_bytes);
                FixedBytes::<32>::from(invalid_root_bytes)
            } else {
                env.compute_output_root_at_block(block).await?
            };

            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (index, address) = env.last_game_info().await?;
            game_addrs.push(address);
            tracing::info!("✓ Created game {index} with parent {parent_id}");
        }

        // Anchor setting step
        for (i, address) in
            game_addrs.into_iter().take(anchor_index.map_or(0, |idx| idx + 1)).enumerate()
        {
            if !invalid_game_ids.contains(&i) {
                env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
                env.resolve_game(address).await?;
                env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
                env.set_anchor_state(address).await?;
                tracing::info!("Anchor game set to index {i}");
            }
        }
        proposer.sync_state().await?;

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(expected_games_len);
        snapshot.assert_anchor_index(anchor_index);
        snapshot.assert_canonical_head(
            expected_canonical_head_index,
            expected_processed_l2_block,
            starting_l2_block,
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_state_with_max_game_deadline_gap() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let mut parent_id = M;
        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut block = starting_l2_block;
        for i in 0..3 {
            block += 1;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (index, address) = env.last_game_info().await?;
            tracing::info!("✓ Created game {index} with parent {M}");

            env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
            env.resolve_game(address).await?;

            if i == 0 {
                env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
            } else {
                env.warp_time(MAX_GAME_DEADLINE_LAG + 1).await?;
            }
            env.set_anchor_state(address).await?;
            tracing::info!("Anchor game set to index {index}");

            parent_id = if parent_id == M { 0 } else { parent_id + 1 };
        }

        proposer.sync_state().await?;

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(2);
        snapshot.assert_anchor_index(Some(2));
        snapshot.assert_canonical_head(Some(2), 3, starting_l2_block);

        Ok(())
    }

    /// Verifies that sync_state correctly filters legacy games with unsupported game types.
    ///
    /// This comprehensive test covers both simple (single legacy game) and complex (multiple
    /// scattered legacy games) scenarios. It creates a mixed sequence of legacy and valid games,
    /// then verifies that:
    /// - sync_state only caches valid games (filters all legacy games)
    /// - canonical head is the latest valid game
    /// - fetch_game returns UnsupportedType for all legacy games
    ///
    /// **When this could happen**:
    /// During a game type transition, a legacy proposer wasn't shut down and keeps creating
    /// legacy-type games
    #[tokio::test]
    async fn test_sync_state_filters_legacy_games() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        // Setup legacy game type infrastructure
        env.setup_legacy_game_type(MOCK_PERMISSIONED_GAME_TYPE, init_bond).await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut block = starting_l2_block;

        // Create mixed sequence: legacy, valid, legacy, valid, valid, legacy
        // EDGE CASE: Simulates a legacy proposer continuing to create games after game type
        // transition. This should NOT happen in practice but tests defensive filtering
        // behavior.
        let game_sequence = [
            (MOCK_PERMISSIONED_GAME_TYPE, false), // index 0: legacy
            (TEST_GAME_TYPE, true),               // index 1: valid
            (MOCK_PERMISSIONED_GAME_TYPE, false), // index 2: legacy
            (TEST_GAME_TYPE, true),               // index 3: valid
            (TEST_GAME_TYPE, true),               // index 4: valid
            (MOCK_PERMISSIONED_GAME_TYPE, false), // index 5: legacy
        ];

        for (game_type, _) in &game_sequence {
            block += 1;
            let root = env.compute_output_root_at_block(block).await?;
            let extra_data = <(U256, u32)>::abi_encode_packed(&(U256::from(block), M));

            if *game_type == MOCK_PERMISSIONED_GAME_TYPE {
                // Create legacy game via factory call
                let create_call = DisputeGameFactory::createCall {
                    _gameType: *game_type,
                    _rootClaim: root,
                    _extraData: Bytes::from(extra_data),
                };
                env.send_factory_tx(create_call.abi_encode(), Some(init_bond)).await?;
            } else {
                // Create valid game via helper
                env.create_game(root, block, M, init_bond).await?;
            }
        }

        // Sync state
        proposer.sync_state().await?;

        // Verify: only 3 valid games should be cached (indices 1, 3, 4)
        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(3);

        // Verify: canonical head should be index 4 (latest valid game)
        snapshot.assert_canonical_head(Some(4), 5, starting_l2_block);

        // Verify: fetch_game on legacy games returns UnsupportedType
        for (index, (_, is_valid)) in game_sequence.iter().enumerate() {
            let fetch_result = proposer.fetch_game(U256::from(index)).await?;
            if *is_valid {
                assert!(
                    matches!(fetch_result, GameFetchResult::AlreadyExists),
                    "Valid game at index {index} should be cached"
                );
            } else {
                assert!(
                    matches!(fetch_result, GameFetchResult::UnsupportedType { .. }),
                    "Legacy game at index {index} should be filtered as UnsupportedType"
                );
            }
        }

        Ok(())
    }

    /// Verifies that sync_state filters games whose type was not respected when created.
    ///
    /// This test creates a game with a non-respected game type, then verifies that:
    /// - sync_state only caches games created with respected type
    /// - fetch_game returns InvalidGame for games with wasRespectedGameTypeWhenCreated = false
    #[tokio::test]
    async fn test_sync_state_filters_non_respected_game_type() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        // Setup legacy game type infrastructure and set it as respected
        env.setup_legacy_game_type(MOCK_PERMISSIONED_GAME_TYPE, init_bond).await?;
        env.set_respected_game_type(MOCK_PERMISSIONED_GAME_TYPE).await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        // Create a valid game with TEST_GAME_TYPE (not respected on creation)
        let valid_block = starting_l2_block + 1;
        let valid_root = env.compute_output_root_at_block(valid_block).await?;
        env.create_game(valid_root, valid_block, M, init_bond).await?;

        // Switch to TEST_GAME_TYPE
        env.set_respected_game_type(TEST_GAME_TYPE).await?;

        // Create another valid game with TEST_GAME_TYPE (respected on creation)
        let valid_block_2 = starting_l2_block + 3;
        let valid_root_2 = env.compute_output_root_at_block(valid_block_2).await?;
        env.create_game(valid_root_2, valid_block_2, M, init_bond).await?;

        // Sync state
        proposer.sync_state().await?;

        // Verify: only the latest valid game should be cached (index 1)
        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(1);

        // Verify: canonical head should be index 1 (latest valid game)
        snapshot.assert_canonical_head(Some(1), 3, starting_l2_block);

        // Verify: fetch_game on non-respected game returns InvalidGame
        let non_respected_fetch_result = proposer.fetch_game(U256::from(0)).await?;
        assert!(
            matches!(non_respected_fetch_result, GameFetchResult::InvalidGame { .. }),
            "Game created with non-respected type should be filtered as InvalidGame"
        );

        // Verify: fetch_game on the latest valid game returns AlreadyExists
        let valid_fetch_result = proposer.fetch_game(U256::from(1)).await?;
        assert!(
            matches!(valid_fetch_result, GameFetchResult::AlreadyExists),
            "Valid game at index 1 should be cached"
        );

        Ok(())
    }

    /// Verifies that games are correctly evicted from cache after bond claims are processed.
    ///
    /// This test covers the eviction logic for games with status DEFENDER_WINS that are:
    /// - Finalized (past DISPUTE_GAME_FINALITY_DELAY_SECONDS)
    /// - Have zero credit (bonds successfully claimed)
    /// - Are NOT protected (anchor game or canonical head)
    ///
    /// Protection rules tested:
    /// - Anchor games are always retained after bond claims
    /// - Canonical head games are always retained after bond claims
    /// - Games that are both anchor and canonical head are retained
    /// - Non-protected games with zero credit are evicted
    ///
    /// This test uses a simple 5-game linear chain (M → 0 → 1 → 2 → 3 → 4) and exercises the
    /// eviction behavior after all games have been resolved as DEFENDER_WINS and all bonds have
    /// been claimed. In this configuration, game 4 becomes both the canonical head and the
    /// on-chain anchor game, so it is the only game retained after eviction.
    #[tokio::test]
    async fn test_bond_claim_cache_eviction() -> Result<()> {
        let expected_retained: Vec<usize> = vec![4];
        let expected_evicted: Vec<usize> = vec![0, 1, 2, 3];

        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        // Step 1: Create 5 games in linear chain M → 0 → 1 → 2 → 3 → 4
        let mut game_addresses = Vec::new();
        let parent_ids = [M, 0, 1, 2, 3];

        for (i, parent_id) in parent_ids.iter().enumerate() {
            let block = starting_l2_block + (i as u64) + 1;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, *parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at {address}");
        }

        // Step 2: Initial sync - all 5 games should be cached
        proposer.sync_state().await?;
        let initial_snapshot = proposer.state_snapshot().await;
        initial_snapshot.assert_game_len(5);
        tracing::info!("✓ All 5 games synced to cache");

        // Step 3: Resolve all games as DEFENDER_WINS
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        for (i, address) in game_addresses.iter().enumerate() {
            env.resolve_game(*address).await?;
            tracing::info!("✓ Resolved game {i} as DEFENDER_WINS");
        }

        // Step 4: Sync after finalization - games should still be retained (have credit)
        proposer.sync_state().await?;
        let pre_claim_snapshot = proposer.state_snapshot().await;
        pre_claim_snapshot.assert_game_len(5);
        tracing::info!("✓ All 5 games still cached after finalization (bonds not yet claimed)");

        // Step 5: Claim bonds for all games (set credit to zero)
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        for (i, address) in game_addresses.iter().enumerate() {
            env.claim_bond(*address, PROPOSER_ADDRESS).await?;
            tracing::info!("✓ Claimed bond for game {i}");
        }

        // Step 6: Sync after claims - eviction should now occur
        proposer.sync_state().await?;

        // Step 7: Verify eviction results
        let final_snapshot = proposer.state_snapshot().await;

        let game_indices: std::collections::HashSet<U256> =
            final_snapshot.games.iter().map(|(idx, _)| *idx).collect();

        // Verify retained games are present
        for idx in &expected_retained {
            assert!(
                game_indices.contains(&U256::from(*idx)),
                "Game {} should be retained (anchor or canonical head)",
                idx
            );
        }

        // Verify evicted games are absent
        for idx in &expected_evicted {
            assert!(
                !game_indices.contains(&U256::from(*idx)),
                "Game {} should be evicted (not anchor, not canonical head, zero credit)",
                idx
            );
        }

        // Verify total count matches expected
        assert_eq!(
            final_snapshot.games.len(),
            expected_retained.len(),
            "Cache should contain exactly {} games after eviction",
            expected_retained.len()
        );

        tracing::info!(
            "✓ Eviction complete: {} games retained, {} games evicted",
            expected_retained.len(),
            expected_evicted.len()
        );

        Ok(())
    }

    /// Verifies that games on different branches are evaluated independently for eviction.
    ///
    /// This test addresses the scenario where:
    /// - Branch A has game i (lower index) that's still in progress
    /// - Branch B has game j (higher index) that's finalized and claimed
    ///
    /// The test ensures that eviction of game j does NOT cause premature eviction of game i.
    ///
    /// Game structure:
    /// - Branch A: M → 0
    /// - Branch B: M → 1
    /// - Branch C: M → 2
    ///
    /// Timeline:
    /// 1. All games created
    /// 2. Branch C game is resolved and bonds claimed → zero credit but not evicted since it
    ///    becomes the anchor game and canonical head.
    /// 3. Branch B game is resolved and bonds claimed → zero credit and evicted.
    /// 4. Branch A game stays in progress and is not evicted.
    #[tokio::test]
    async fn test_bond_claim_eviction_multi_branch() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        let mut game_addresses = Vec::new();

        for i in 0..3 {
            let block = starting_l2_block + (i as u64) + 1;
            let root = env.compute_output_root_at_block(block).await?;
            env.create_game(root, block, M, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at {address}");
        }

        // Initial sync - all 3 games should be cached
        proposer.sync_state().await?;
        let initial_snapshot = proposer.state_snapshot().await;
        initial_snapshot.assert_game_len(3);
        tracing::info!("✓ All 3 games synced to cache");

        // Resolve for Branch B and C games
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(game_addresses[1]).await?;
        tracing::info!("✓ Resolved game 1");

        env.resolve_game(game_addresses[2]).await?;
        tracing::info!("✓ Resolved game 2");

        // Claim bonds for Branch C game
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[2], PROPOSER_ADDRESS).await?;
        tracing::info!("✓ Claimed bond for game 2");

        // Sync after claim for Branch C game
        proposer.sync_state().await?;
        let pre_claim_snapshot = proposer.state_snapshot().await;
        pre_claim_snapshot.assert_game_len(3);
        tracing::info!("✓ All 3 games still cached after claim for Branch C game");

        // Claim bonds for Branch B game
        env.claim_bond(game_addresses[1], PROPOSER_ADDRESS).await?;
        tracing::info!("✓ Claimed bond for game 1");

        // Sync after claim for Branch B game
        proposer.sync_state().await?;
        let pre_claim_snapshot = proposer.state_snapshot().await;
        pre_claim_snapshot.assert_game_len(2);
        tracing::info!("✓ Branch B game should be evicted but Branch A game should be retained");

        // Verify eviction results
        let final_snapshot = proposer.state_snapshot().await;

        let game_indices: std::collections::HashSet<U256> =
            final_snapshot.games.iter().map(|(idx, _)| *idx).collect();

        // Game 1 should be evicted: not anchor, not canonical head, zero credit
        assert!(
            !game_indices.contains(&U256::from(1)),
            "Game 1 should be evicted (branch B, zero credit, not protected)"
        );

        // Game 0 should be retained: on branch A, still in progress
        assert!(
            game_indices.contains(&U256::from(0)),
            "Game 0 should be retained (branch A, still in progress)"
        );

        // Game 2 should be retained: anchor game (highest block)
        assert!(
            game_indices.contains(&U256::from(2)),
            "Game 2 should be retained (anchor game and canonical head on branch C)"
        );

        // Verify canonical head is game 2 (highest block)
        final_snapshot.assert_canonical_head(Some(2), 3, starting_l2_block);

        tracing::info!(
            "✓ Multi-branch eviction verified: game 1 (index 1) evicted, game 0 (index 0) retained"
        );

        Ok(())
    }

    /// Topology: three root siblings with distinct states.
    /// M -> 0 (DEFENDER_WINS, credit → 0), M -> 1 (CHALLENGER_WINS), M -> 2 (DEFENDER_WINS, credit
    /// → 0).
    /// Game 2 is finalized and becomes anchor, so Game 0 is evicted.
    #[tokio::test]
    async fn test_three_sibling_states_eviction_and_pruning() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut game_addresses = Vec::new();

        // Create three root games with increasing blocks.
        for i in 0..3 {
            let block = starting_l2_block + 1 + i as u64;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, M, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block}");
        }

        // Challenge game 1
        env.challenge_game(game_addresses[1]).await?;

        // Game 2: resolve as DEFENDER_WINS, finalize, then claim to set credit = 0
        // Game 2 becomes anchor and will evict game 0 on next sync
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(game_addresses[2]).await?;

        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[2], PROPOSER_ADDRESS).await?;

        // Game 0: resolve as DEFENDER_WINS, finalize, then claim to set credit = 0
        env.resolve_game(game_addresses[0]).await?;

        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[0], PROPOSER_ADDRESS).await?;

        // Game 1: make CHALLENGER_WINS
        env.warp_time(MAX_PROVE_DURATION + 1).await?;
        env.resolve_game(game_addresses[1]).await?;

        proposer.sync_state().await?;

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(1);
        snapshot.assert_anchor_index(Some(2));
        snapshot.assert_canonical_head(Some(2), 3, starting_l2_block);

        let game_indices: std::collections::HashSet<U256> =
            snapshot.games.iter().map(|(idx, _)| *idx).collect();
        assert!(
            game_indices.contains(&U256::from(2)),
            "Anchor game should be retained even if it is finalized and has zero credit"
        );
        assert!(
            !game_indices.contains(&U256::from(0)),
            "Zero-credit DEFENDER_WINS should be evicted if not anchor or canonical head"
        );
        assert!(!game_indices.contains(&U256::from(1)), "CHALLENGER_WINS branch should be pruned");

        Ok(())
    }

    /// Topology: M -> 0 (anchor, DEFENDER_WINS, credit > 0); 0 -> 1 (DEFENDER_WINS, credit → 0);
    /// 0 -> 2 (IN_PROGRESS). Ensures the zero-credit branch is evicted while the anchor and the
    /// live sibling are retained and canonical head sits on the in-progress child.
    #[tokio::test]
    async fn test_anchor_with_zero_credit_sibling_and_in_progress_branch() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut game_addresses = Vec::new();

        // Create anchor root (0) and three children (1 - zero-credit, 2 - zero-credit, 3 -
        // in-progress)
        for i in 0..4 {
            let block = starting_l2_block + 1 + i as u64;
            let root_claim = env.compute_output_root_at_block(block).await?;
            let parent_id = if i == 0 { M } else { 0 };
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block} with parent {parent_id}");
        }

        // Anchor game 0
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(game_addresses[0]).await?;
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[0], PROPOSER_ADDRESS).await?;

        // Resolve game 2 and claim its bond to set credit = 0
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(game_addresses[2]).await?;
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[2], PROPOSER_ADDRESS).await?;

        // Resolve game 1 and claim its bond to set credit = 0 (expected to be evicted; it's not
        // the anchor or canonical head)
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(game_addresses[1]).await?;
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[1], PROPOSER_ADDRESS).await?;

        // Game 3 is left IN_PROGRESS
        proposer.sync_state().await?;

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(2);
        snapshot.assert_anchor_index(Some(2));
        snapshot.assert_canonical_head(Some(2), 3, starting_l2_block);

        let game_indices: std::collections::HashSet<U256> =
            snapshot.games.iter().map(|(idx, _)| *idx).collect();

        assert!(!game_indices.contains(&U256::from(0)), "Old anchor game should be evicted");
        assert!(!game_indices.contains(&U256::from(1)), "Zero-credit game should be evicted");
        assert!(game_indices.contains(&U256::from(2)), "Anchor game should be retained");
        assert!(game_indices.contains(&U256::from(3)), "In-progress game should be retained");

        // Verify credit balances
        let anchor_credit = env.get_credit(game_addresses[2], PROPOSER_ADDRESS).await?;
        assert_eq!(anchor_credit, U256::ZERO, "Anchor game should have zero credit");

        let game_3_credit = env.get_credit(game_addresses[3], PROPOSER_ADDRESS).await?;
        assert_ne!(game_3_credit, U256::ZERO, "Game 3 should have non-zero credit");

        Ok(())
    }

    /// Topology: M -> 0 (CHALLENGER_WINS) -> 1, M -> 2 (DEFENDER_WINS, credit → 0), M -> 3
    /// (anchor). Combines pruning of the challenged branch (0,1) with eviction of the
    /// zero-credit branch (2) while retaining the anchor branch (3). Canonical head should stay
    /// on the anchor.
    #[tokio::test]
    async fn test_anchor_with_challenger_pruning_and_zero_credit_eviction() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let parent_ids = [M, 0, M, M]; // 0: challenged root, 1: its child, 2: zero-credit branch, 3: anchor

        let mut game_addresses = Vec::new();
        let mut block = starting_l2_block;

        // Create four games with increasing L2 blocks
        for (i, parent_id) in parent_ids.iter().enumerate() {
            block += 1;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, *parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block} with parent {parent_id}");
        }

        // Challenge the parallel branch (game 0)
        env.challenge_game(game_addresses[0]).await?;

        // Resolve and anchor game 3
        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(game_addresses[3]).await?;
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.set_anchor_state(game_addresses[3]).await?;

        // Resolve challenged branch root and the zero-credit branch
        env.warp_time(MAX_PROVE_DURATION + 1).await?;
        env.resolve_game(game_addresses[0]).await?; // becomes CHALLENGER_WINS
        env.resolve_game(game_addresses[2]).await?; // DEFENDER_WINS

        // Finalize and claim bond for game 2 to drive credit to zero
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        env.claim_bond(game_addresses[2], PROPOSER_ADDRESS).await?;

        // Sync and assert outcomes: challenged branch pruned, zero-credit branch evicted, anchor
        // retained
        proposer.sync_state().await?;
        let snapshot = proposer.state_snapshot().await;

        snapshot.assert_game_len(1);
        snapshot.assert_anchor_index(Some(3));
        snapshot.assert_canonical_head(Some(3), 4, starting_l2_block);

        let game_indices: std::collections::HashSet<U256> =
            snapshot.games.iter().map(|(idx, _)| *idx).collect();

        assert!(game_indices.contains(&U256::from(3)), "Anchor game should be retained");
        assert!(!game_indices.contains(&U256::from(0)), "Challenged branch should be pruned");
        assert!(
            !game_indices.contains(&U256::from(1)),
            "Child of challenged branch should be pruned"
        );
        assert!(
            !game_indices.contains(&U256::from(2)),
            "Zero-credit DEFENDER_WINS branch should be evicted"
        );

        Ok(())
    }

    /// Tests CHALLENGER_WINS cascade removal at various tree positions.
    #[rstest]
    #[case::at_root(
        &[M, 0, 1],  // parent_ids: M -> 0 -> 1 -> 2
        &[0],  // games_to_challenge: challenge game 0
        None,  // anchor_id: no anchor
        0,  // expected_retained_count (all removed - root is CHALLENGER_WINS)
        None,
        0
    )]
    #[case::mid_chain(
        &[M, 0, 1, 2],  // M -> 0 -> 1 -> 2 -> 3
        &[1],  // challenge game 1
        None,  // anchor_id: no anchor
        1,  // Only game 0 retained (games 1, 2, 3 removed)
        Some(0),
        1
    )]
    #[case::two_children(
        &[M, 0, 0],  // M -> 0, 0 -> 1, 0 -> 2
        &[1],  // challenge game 1
        None,  // anchor_id: no anchor
        2,  // Games 0, 2 retained; game 1 removed
        Some(2),
        3
    )]
    #[case::multiple_challenger_wins(
        &[M, 0, 1],  // M -> 0 -> 1 -> 2
        &[0, 1],  // challenge games 0 and 1
        None,  // anchor_id: no anchor
        0,  // All removed
        None,
        0
    )]
    #[case::anchor_descendant_challenger_wins(
        &[M, 0, 1],  // M -> 0 (anchor) -> 1 -> 2
        &[1],  // challenge game 1 (descendant of anchor)
        Some(0),  // anchor_id: game 0
        1,  // Only game 0 retained (games 1, 2 removed despite anchor)
        Some(0),  // Canonical head = anchor
        1
    )]
    #[case::parallel_branch_challenger_wins(
        &[M, 0, M],  // M -> 0 (anchor) -> 1; M -> 2
        &[2],  // challenge game 2 (parallel branch)
        Some(0),  // anchor_id: game 0
        2,  // Games 0, 1 retained; game 2 removed
        Some(1),  // Canonical head = game 1
        2
    )]
    #[case::multiple_challenger_wins_different_branches(
        &[M, 0, 1, 0, 3],  // M -> 0 -> 1 -> 2; 0 -> 3 -> 4
        &[1, 3],  // challenge games 1 and 3 (different branches)
        None,  // anchor_id: no anchor
        1,  // Only game 0 retained (both branches pruned)
        Some(0),
        1
    )]
    #[case::anchor_parallel_challenger_wins_with_child(
        &[M, 0, M, 2],  // M -> 0 (anchor) -> 1; M -> 2 (challenged) -> 3
        &[2],  // challenge game 2 (newer parallel branch with child)
        Some(0),  // anchor_id: game 0
        2,  // Games 0, 1 retained; games 2, 3 removed
        Some(1),  // Canonical head stays on anchor branch
        2
    )]
    #[tokio::test]
    async fn test_challenger_wins_cascade_removal(
        #[case] parent_ids: &[u32],
        #[case] games_to_challenge: &[usize],
        #[case] anchor_id: Option<usize>,
        #[case] expected_retained_count: usize,
        #[case] expected_canonical_head_index: Option<u64>,
        #[case] expected_processed_l2_block: u64,
    ) -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let mut block = starting_l2_block;
        let mut game_addresses = Vec::new();

        // Step 1: Create all games with valid roots (all initially valid)
        for (i, &parent_id) in parent_ids.iter().enumerate() {
            block += 1;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block} (parent: {parent_id})");
        }

        // Step 2: Challenge specific games
        for &idx in games_to_challenge {
            env.challenge_game(game_addresses[idx]).await?;
            tracing::info!("✓ Game {idx} challenged");
        }

        // Step 3: Set anchor if specified
        if let Some(anchor_idx) = anchor_id {
            // Resolve anchor game as DEFENDER_WINS first
            env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
            env.resolve_game(game_addresses[anchor_idx]).await?;
            tracing::info!("✓ Resolved game {anchor_idx} as DEFENDER_WINS for anchor");

            // Set as anchor (requires finality delay)
            env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
            env.set_anchor_state(game_addresses[anchor_idx]).await?;
            tracing::info!("✓ Set game {anchor_idx} as anchor");
        }

        // Step 4: Resolve games (skip anchor if already resolved)
        env.warp_time(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION + 1).await?;
        for (i, game_address) in game_addresses.iter().enumerate() {
            // Skip if already resolved for anchor
            if anchor_id != Some(i) {
                env.resolve_game(*game_address).await?;
                tracing::info!("✓ Resolved game {game_address}");
            }
        }

        // Step 5: Sync state
        proposer.sync_state().await?;

        // Verify results
        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(expected_retained_count);
        snapshot.assert_anchor_index(anchor_id);
        snapshot.assert_canonical_head(
            expected_canonical_head_index,
            expected_processed_l2_block,
            starting_l2_block,
        );

        for &idx in games_to_challenge {
            assert!(
                !snapshot.games.iter().any(|(key, _)| *key == U256::from(idx)),
                "Game {idx} should be removed (challenged)"
            );
        }

        Ok(())
    }

    /// Tests mixed states across multiple branches.
    ///
    /// Topology:
    ///   M → 0 (DEFENDER_WINS) → 1 (DEFENDER_WINS) → 2 (DEFENDER_WINS)
    ///     → 0 → 3 (IN_PROGRESS, Challenged) → 4 (IN_PROGRESS)
    ///     → 1 → 5 (IN_PROGRESS)
    ///
    /// Expected:
    /// - Games 0, 1, 2, 5 retained (valid chain)
    /// - Games 3, 4 removed (CHALLENGER_WINS cascade)
    /// - Canonical head: Game 5 (highest L2 block among valid games)
    #[tokio::test]
    async fn test_mixed_states_multiple_branches() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        // Step 1: Create all 6 games with valid roots and proper parent relationships
        let mut game_addresses = Vec::new();
        let parent_ids = [M, 0, 1, 0, 3, 1]; // parents for games 0-5

        for (i, &parent_id) in parent_ids.iter().enumerate() {
            let block = starting_l2_block + 1 + i as u64;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block} with parent {parent_id}");
        }

        // Step 2: Challenge game 3 to make it CHALLENGER_WINS
        env.challenge_game(game_addresses[3]).await?;
        tracing::info!("✓ Game 3 challenged");

        // Step 3: Resolve games 0, 1 and 2 as DEFENDER_WINS
        env.warp_time(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION + 1).await?;
        for game_address in game_addresses.iter().take(3) {
            env.resolve_game(*game_address).await?;
            tracing::info!("✓ Game {game_address} resolved");
        }

        // Step 4: Sync state
        proposer.sync_state().await?;

        // Verify: All games are cached
        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(6);

        // Verify: Canonical head is game 5 (highest L2 block among valid games)
        snapshot.assert_canonical_head(Some(5), 6, starting_l2_block);

        // Step 5: Resolve game 3, 4 and 5
        for game_address in game_addresses.iter().skip(3) {
            env.resolve_game(*game_address).await?;
            tracing::info!("✓ Game {game_address} resolved");
        }

        // Step 6: Sync state
        proposer.sync_state().await?;

        // Verify: Games 0, 1, 2, 5 retained; games 3, 4 removed
        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_game_len(4); // 0, 1, 2, 5

        // Canonical head should be game 5 (highest block among reachable games)
        snapshot.assert_canonical_head(Some(5), 6, starting_l2_block);

        // Verify specific games are present
        let game_indices: Vec<U256> = snapshot.games.iter().map(|(idx, _)| *idx).collect();
        assert!(game_indices.contains(&U256::from(0)), "Game 0 should be retained");
        assert!(game_indices.contains(&U256::from(1)), "Game 1 should be retained");
        assert!(game_indices.contains(&U256::from(2)), "Game 2 should be retained");
        assert!(game_indices.contains(&U256::from(5)), "Game 5 should be retained");
        assert!(
            !game_indices.contains(&U256::from(3)),
            "Game 3 should be removed (CHALLENGER_WINS)"
        );
        assert!(
            !game_indices.contains(&U256::from(4)),
            "Game 4 should be removed (child of CHALLENGER_WINS)"
        );

        Ok(())
    }

    /// Tests the `should_attempt_to_resolve` flag logic for IN_PROGRESS games.
    ///
    /// Verifies that the flag is correctly set based on:
    /// - Game status (IN_PROGRESS vs resolved)
    /// - Deadline status (passed vs not passed)
    /// - Parent resolution status (resolved vs IN_PROGRESS)
    #[tokio::test]
    async fn test_in_progress_games_resolution_marking() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        // Create a simple chain: M -> 0 -> 1 -> 2 -> 3 -> 4
        // Add 100-second gaps between creations to control deadline spacing
        let mut game_addresses = Vec::new();
        let time_gap = 100u64;

        let total_games = 5;
        let num_time_gaps = total_games - 1;

        for i in 0..total_games {
            let block = starting_l2_block + 1 + i;
            let root_claim = env.compute_output_root_at_block(block).await?;
            let parent_id = if i == 0 { M } else { (i - 1) as u32 };
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block} with parent {parent_id}");

            // Add time gap before next game (except after last game)
            if i < num_time_gaps {
                env.warp_time(time_gap).await?;
            }
        }

        // Sync state to cache games
        proposer.sync_state().await?;

        // === Case 1: IN_PROGRESS game with deadline NOT passed ===
        let game_0 = cached_game(&proposer, 0).await?;
        assert!(
            !game_0.should_attempt_to_resolve,
            "Game 0: should_attempt_to_resolve should be false (deadline not passed)"
        );
        tracing::info!("✓ Case 1: IN_PROGRESS + deadline not passed → should_attempt = false");

        // === Case 2: DEFENDER_WINS (resolved game) ===
        // Warp to just past game 0's deadline but before game 1's deadline
        // Game 0 deadline is at T0 + MAX_CHALLENGE_DURATION
        // Game 1 deadline is at T0 + 100 + MAX_CHALLENGE_DURATION
        // Currently at T0 + 400 (after creating all `total_games` games with `time_gap`-second
        // gaps)
        // So we need to warp: MAX_CHALLENGE_DURATION - `num_time_gaps` * `time_gap` + 1
        env.warp_time(MAX_CHALLENGE_DURATION - num_time_gaps * time_gap + 1).await?;

        proposer.sync_state().await?;
        let game_0 = cached_game(&proposer, 0).await?;
        assert!(
            game_0.should_attempt_to_resolve,
            "Game 0: should_attempt_to_resolve should be true since deadline has passed"
        );

        env.resolve_game(game_addresses[0]).await?;
        proposer.sync_state().await?;

        let game_0 = cached_game(&proposer, 0).await?;
        assert!(
            !game_0.should_attempt_to_resolve,
            "Game 0: should_attempt_to_resolve should be false (already resolved)"
        );
        tracing::info!("✓ Case 2: DEFENDER_WINS → should_attempt = false");

        // === Case 3: IN_PROGRESS + parent resolved + deadline NOT passed ===
        let game_1 = cached_game(&proposer, 1).await?;
        assert!(
            !game_1.should_attempt_to_resolve,
            "Game 1: should_attempt_to_resolve should be false (deadline not passed despite parent resolved)"
        );
        tracing::info!(
            "✓ Case 3: IN_PROGRESS + parent resolved + deadline not passed → should_attempt = false"
        );

        // === Case 4: IN_PROGRESS + parent resolved + deadline passed + own game ===
        env.warp_time(time_gap).await?;
        proposer.sync_state().await?;

        let game_1 = cached_game(&proposer, 1).await?;
        assert!(
            game_1.should_attempt_to_resolve,
            "Game 1: should_attempt_to_resolve should be true (all conditions met)"
        );
        tracing::info!(
            "✓ Case 4: IN_PROGRESS + parent resolved + deadline passed + own game → should_attempt = true"
        );

        // === Case 5: IN_PROGRESS + parent NOT resolved + deadline passed ===
        env.warp_time(time_gap).await?;
        let game_2 = cached_game(&proposer, 2).await?;
        assert!(
            !game_2.should_attempt_to_resolve,
            "Game 2: should_attempt_to_resolve should be false (parent not resolved)"
        );
        tracing::info!(
            "✓ Case 5: IN_PROGRESS + parent NOT resolved + deadline passed → should_attempt = false"
        );

        env.resolve_game(game_addresses[1]).await?;
        env.resolve_game(game_addresses[2]).await?;
        proposer.sync_state().await?;

        // === Case 6: IN_PROGRESS + UnchallengedAndValidProofProvided ===
        env.prove_game(game_addresses[3]).await?;
        proposer.sync_state().await?;

        let game_3 = cached_game(&proposer, 3).await?;
        assert_eq!(game_3.proposal_status, ProposalStatus::UnchallengedAndValidProofProvided);
        assert!(
            game_3.should_attempt_to_resolve,
            "Game 3: should_attempt_to_resolve should be true (proof provided)"
        );
        tracing::info!(
            "✓ Case 6: IN_PROGRESS + UnchallengedAndValidProofProvided → should_attempt = true"
        );

        env.resolve_game(game_addresses[3]).await?;
        proposer.sync_state().await?;
        // === Case 7: IN_PROGRESS + ChallengedAndValidProofProvided ===
        env.challenge_game(game_addresses[4]).await?;
        proposer.sync_state().await?;

        let game_4 = cached_game(&proposer, 4).await?;
        assert_eq!(game_4.proposal_status, ProposalStatus::Challenged);
        assert!(
            !game_4.should_attempt_to_resolve,
            "Game 4: should_attempt_to_resolve should be false (challenged)"
        );

        env.prove_game(game_addresses[4]).await?;
        proposer.sync_state().await?;
        let game_4 = cached_game(&proposer, 4).await?;
        assert_eq!(game_4.proposal_status, ProposalStatus::ChallengedAndValidProofProvided);
        assert!(
            game_4.should_attempt_to_resolve,
            "Game 4: should_attempt_to_resolve should be true (proof provided)"
        );

        Ok(())
    }

    /// Topology: M -> 0 (DEFENDER_WINS), 0 -> 1 (Challenged), 0 -> 2
    /// (UnchallengedAndValidProofProvided). Verifies proposal status flags across branches in a
    /// single sync pass.
    #[tokio::test]
    async fn test_in_progress_proposal_status_multi_branch() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        // Create root (0) and two children (1, 2) with increasing blocks.
        let mut game_addresses = Vec::new();
        let parent_ids = [M, 0, 0];

        for (i, &parent_id) in parent_ids.iter().enumerate() {
            let block = starting_l2_block + 1 + i as u64;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, parent_id, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block} with parent {parent_id}");
        }

        // Branch 1: mark as Challenged.
        env.challenge_game(game_addresses[1]).await?;

        // Branch 2: provide proof (UnchallengedAndValidProofProvided) without challenge.
        env.prove_game(game_addresses[2]).await?;

        // Resolve root as DEFENDER_WINS.
        env.warp_time(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION + 1).await?;
        env.resolve_game(game_addresses[0]).await?;

        // Sync and inspect per-branch flags.
        proposer.sync_state().await?;

        let game_1 = proposer.get_game(U256::from(1)).await.unwrap();
        assert_eq!(game_1.proposal_status, ProposalStatus::Challenged);
        assert!(!game_1.should_attempt_to_resolve, "Challenged game should not auto-resolve");

        let game_2 = proposer.get_game(U256::from(2)).await.unwrap();
        assert_eq!(game_2.proposal_status, ProposalStatus::UnchallengedAndValidProofProvided);
        assert!(game_2.should_attempt_to_resolve, "Proof-provided game should attempt to resolve");

        let snapshot = proposer.state_snapshot().await;
        snapshot.assert_canonical_head(Some(2), 3, starting_l2_block);

        Ok(())
    }

    /// Tests the `should_attempt_to_claim_bond` flag logic for finalized games.
    ///
    /// Verifies that the flag is correctly set based on:
    /// - Finality (finalized vs not finalized)
    /// - Credit availability (credit > 0 vs credit = 0)
    #[tokio::test]
    async fn test_bond_claim_marking() -> Result<()> {
        let (env, proposer, init_bond) = setup().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;

        let block = starting_l2_block + 1;
        let root_claim = env.compute_output_root_at_block(block).await?;
        env.create_game(root_claim, block, M, init_bond).await?;
        let (_, address) = env.last_game_info().await?;
        tracing::info!("✓ Created game 0 at block {block}");

        env.warp_time(MAX_CHALLENGE_DURATION + 1).await?;
        env.resolve_game(address).await?;
        tracing::info!("✓ Resolved game 0 as DEFENDER_WINS");

        // Sync state
        proposer.sync_state().await?;

        // === Case 1: DEFENDER_WINS + not finalized ===
        let game_0 = cached_game(&proposer, 0).await?;
        assert!(
            !game_0.should_attempt_to_claim_bond,
            "Game 0: should_attempt_to_claim_bond should be false (not finalized)"
        );
        tracing::info!(
            "✓ Case 1: DEFENDER_WINS + not finalized → should_attempt_to_claim_bond = false"
        );

        // Warp time to finalize games
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        proposer.sync_state().await?;

        // === Case 2: DEFENDER_WINS + finalized + credit > 0 ===
        let game_0 = cached_game(&proposer, 0).await?;
        assert!(
            game_0.should_attempt_to_claim_bond,
            "Game 0: should_attempt_to_claim_bond should be true (finalized + credit > 0)"
        );
        tracing::info!("✓ Case 2: DEFENDER_WINS + finalized + credit > 0 → should_attempt_to_claim_bond = true");

        // Claim bond for game 0 to set credit = 0
        env.claim_bond(address, PROPOSER_ADDRESS).await?;
        proposer.sync_state().await?;

        // === Case 3: DEFENDER_WINS + finalized + credit = 0 ===
        let game_0 = cached_game(&proposer, 0).await?;
        assert!(
            !game_0.should_attempt_to_claim_bond,
            "Game 0: should_attempt_to_claim_bond should be false (credit = 0)"
        );
        tracing::info!("✓ Case 3: DEFENDER_WINS + finalized + credit = 0 → should_attempt_to_claim_bond = false");

        Ok(())
    }

    /// Verifies that defense tasks are spawned in deadline-ascending order.
    ///
    /// This test creates 2 games, then challenges them in reverse order (game 1 first, then
    /// game 0). Since the deadline is reset to `challenge_time + MAX_PROVE_DURATION` on challenge,
    /// the game challenged first (game 1) will have the earliest deadline.
    ///
    /// Timeline:
    /// - Game 0: Created at T0
    /// - Game 1: Created at T0 + 100s
    /// - Game 1: Challenged first → deadline = T_challenge + MAX_PROVE_DURATION
    /// - Game 0: Challenged second → deadline = (T_challenge + ε) + MAX_PROVE_DURATION
    ///
    /// Result: game_1.deadline < game_0.deadline
    /// Expected defense order: Game 1 first (earliest deadline)
    ///
    /// Note: This test only verifies that defense tasks are spawned for challenged games with
    /// correct deadline ordering. It does not wait for proving to complete, as the actual proving
    /// would fail in the Anvil test environment (no real L1 RPC).
    #[tokio::test]
    async fn test_defense_tasks_prioritize_earliest_deadline() -> Result<()> {
        let env = TestEnvironment::setup().await?;
        let factory = env.factory()?;
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        let proposer = env.init_proposer().await?;

        let starting_l2_block = env.anvil.starting_l2_block_number;
        let time_gap = 100u64; // 100 seconds between game creations
        let mut game_addresses = Vec::new();

        // Create 2 games with increasing deadlines (time gap between creation)
        for i in 0..2u64 {
            let block = starting_l2_block + 1 + i;
            let root_claim = env.compute_output_root_at_block(block).await?;
            env.create_game(root_claim, block, M, init_bond).await?;
            let (_, address) = env.last_game_info().await?;
            game_addresses.push(address);
            tracing::info!("✓ Created game {i} at block {block}");

            // Add time gap before next game (except after last game)
            if i < 1 {
                env.warp_time(time_gap).await?;
            }
        }

        // Challenge games in REVERSE creation order (game 1 first, then game 0).
        // Since deadline = challenge_time + MAX_PROVE_DURATION, game 1 will have the earlier
        // deadline.
        env.challenge_game(game_addresses[1]).await?;
        tracing::info!("✓ Challenged game 1 FIRST (will have earlier deadline)");

        env.challenge_game(game_addresses[0]).await?;
        tracing::info!("✓ Challenged game 0 SECOND (will have later deadline)");

        // Sync state to update proposal status
        proposer.sync_state().await?;

        // Verify both games are challenged
        for i in 0..2u64 {
            let game = proposer.get_game(U256::from(i)).await.unwrap();
            assert_eq!(
                game.proposal_status,
                ProposalStatus::Challenged,
                "Game {i} should be challenged"
            );
        }

        // Verify deadlines are in expected order (game 1 < game 0)
        let game_0 = proposer.get_game(U256::from(0)).await.unwrap();
        let game_1 = proposer.get_game(U256::from(1)).await.unwrap();

        assert!(
            game_1.deadline < game_0.deadline,
            "Game 1 should have earlier deadline than game 0: {} < {}",
            game_1.deadline,
            game_0.deadline
        );

        tracing::info!("Deadlines - Game 0: {}, Game 1: {}", game_0.deadline, game_1.deadline);

        // Spawn defense tasks - with max_concurrent_defense_tasks=1, only the earliest deadline
        // game (game 1) should be selected for defense.
        let spawned = proposer.spawn_defense_tasks_for_test().await?;
        assert!(spawned, "Should have spawned at least one defense task");

        // Verify the defense task was spawned for game 1 (earliest deadline).
        let active_defense_addresses = proposer.get_active_defense_game_addresses().await;
        assert_eq!(
            active_defense_addresses.len(),
            1,
            "Should have exactly 1 active defense task (max_concurrent_defense_tasks=1)"
        );
        assert_eq!(
            active_defense_addresses[0], game_addresses[1],
            "Defense task should be for game 1 (earliest deadline)"
        );

        tracing::info!("✓ Defense task correctly spawned for game 1: {:?}", game_addresses[1]);

        Ok(())
    }
}
