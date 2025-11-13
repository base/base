pub mod common;

#[cfg(feature = "e2e")]
mod sync {
    use std::collections::HashMap;

    use crate::common::{
        constants::{DISPUTE_GAME_FINALITY_DELAY_SECONDS, MAX_CHALLENGE_DURATION, TEST_GAME_TYPE},
        TestEnvironment,
    };
    use alloy_primitives::{FixedBytes, Uint, U256};
    use anyhow::Result;
    use fault_proof::proposer::{
        GameFetchResult, OPSuccinctProposer, ProposerStateSnapshot, MAX_GAME_DEADLINE_LAG,
    };
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
}
