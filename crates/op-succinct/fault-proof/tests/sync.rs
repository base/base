pub mod common;

#[cfg(feature = "e2e")]
mod sync {
    use std::collections::HashMap;

    use crate::common::{
        constants::{
            DISPUTE_GAME_FINALITY_DELAY_SECONDS, MAX_CHALLENGE_DURATION,
            MOCK_PERMISSIONED_GAME_TYPE, PROPOSER_ADDRESS, TEST_GAME_TYPE,
        },
        TestEnvironment,
    };
    use alloy_primitives::{Bytes, FixedBytes, Uint, U256};
    use alloy_sol_types::{SolCall, SolValue};
    use anyhow::Result;
    use fault_proof::proposer::{
        GameFetchResult, OPSuccinctProposer, ProposerStateSnapshot, MAX_GAME_DEADLINE_LAG,
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
    #[case::anch_two_games_same_parent_diff_intervals(2, &[0], &[M, M], &[1, 2], Some(0), 1)]
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
}
