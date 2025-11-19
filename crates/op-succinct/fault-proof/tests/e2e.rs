pub mod common;

#[cfg(feature = "e2e")]
mod e2e {
    use super::*;
    use std::sync::Arc;

    use alloy_primitives::{Bytes, FixedBytes, U256};
    use alloy_sol_types::{SolCall, SolValue};
    use anyhow::{Context, Result};
    use common::{
        constants::{
            CHALLENGER_ADDRESS, DISPUTE_GAME_FINALITY_DELAY_SECONDS, MAX_CHALLENGE_DURATION,
            MAX_PROVE_DURATION, MOCK_PERMISSIONED_GAME_TYPE, PROPOSER_ADDRESS, TEST_GAME_TYPE,
        },
        monitor::{verify_all_resolved_correctly, TrackedGame},
        TestEnvironment,
    };
    use fault_proof::{
        challenger::Game,
        contract::{GameStatus, ProposalStatus},
    };
    use op_succinct_bindings::dispute_game_factory::DisputeGameFactory;
    use rand::Rng;
    use tokio::time::{sleep, Duration};
    use tracing::info;

    use crate::common::init_challenger;

    alloy_sol_types::sol! {
        #[sol(rpc)]
        contract MockPermissionedDisputeGameView {
            function status() external view returns (uint8);
        }
    }

    // Covers the proposer happy path from creation through resolution to bond claim.
    // Ensures the proposer keeps running and completes every stage cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_honest_proposer_native() -> Result<()> {
        info!("=== Test: Honest Proposer Full Lifecycle (Create → Resolve → Claim) ===");

        let env = TestEnvironment::setup().await?;

        let proposer_handle = env.start_proposer().await?;

        // Wait for proposer to create games
        info!("=== Waiting for Game Creation ===");

        // Track first 3 games (L2 finalized head won't advance far enough for 3)
        let tracked_games = env.wait_and_track_games(3, 30).await?;
        info!("✓ Proposer created {} games:", tracked_games.len());
        for (i, game) in tracked_games.iter().enumerate() {
            info!("  Game {}: {} at L2 block {}", i + 1, game.address, game.l2_block_number);
        }

        // Verify proposer is still running
        assert!(!proposer_handle.is_finished(), "Proposer should still be running");
        info!("✓ Proposer is still running successfully");

        // === PHASE 2: Challenge Period ===
        info!("=== Phase 2: Challenge Period ===");

        // Warp by max challenge duration
        env.warp_time(MAX_CHALLENGE_DURATION).await?;
        info!("✓ Warped time by max challenge duration ({MAX_CHALLENGE_DURATION} seconds) to trigger resolution");

        // Verify proposer is still running
        assert!(!proposer_handle.is_finished(), "Proposer should still be running");
        info!("✓ Proposer is still running successfully");

        // === PHASE 3: Resolution ===
        info!("=== Phase 3: Resolution ===");

        // Wait for games to be resolved
        let resolutions = env.wait_for_resolutions(&tracked_games, 30).await?;

        // Verify all games resolved correctly (proposer wins)
        verify_all_resolved_correctly(&resolutions)?;

        // Warp past DISPUTE_GAME_FINALITY_DELAY_SECONDS
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS).await?;
        info!("✓ Warped time by DISPUTE_GAME_FINALITY_DELAY_SECONDS ({DISPUTE_GAME_FINALITY_DELAY_SECONDS} seconds) to trigger bond claims");

        // Verify proposer is still running
        assert!(!proposer_handle.is_finished(), "Proposer should still be running");
        info!("✓ Proposer is still running successfully");

        // === PHASE 4: Bond Claims ===
        info!("=== Phase 4: Bond Claims ===");

        // Wait for proposer to claim bonds
        env.wait_for_bond_claims(&tracked_games, PROPOSER_ADDRESS, 30).await?;

        env.stop_proposer(proposer_handle);

        info!("=== Full Lifecycle Test Complete ===");
        info!("✓ Games created, resolved, and bonds claimed successfully");

        Ok(())
    }

    // Seeds a legacy game, transitions game types, and verifies the proposer ignores it.
    // Confirms new games are created, resolved, and bonds claimed despite history.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_type_transition_skips_legacy_game() -> Result<()> {
        info!("=== Test: Game Type Transition With Legacy Game In History ===");

        let env = TestEnvironment::setup().await?;

        let factory = env.factory()?;
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        // Setup legacy game type and set it as respected
        env.setup_legacy_game_type(MOCK_PERMISSIONED_GAME_TYPE, init_bond).await?;
        env.set_respected_game_type(MOCK_PERMISSIONED_GAME_TYPE).await?;

        let initial_game_count = factory.gameCount().call().await?;
        let mut expected_index = initial_game_count;
        info!("Seeding legacy game (type {MOCK_PERMISSIONED_GAME_TYPE})");

        let interval = 10;
        let l2_block = env.anvil.starting_l2_block_number + interval;
        let root_claim = env.compute_output_root_at_block(l2_block).await?;
        let extra_data = <(U256, u32)>::abi_encode_packed(&(U256::from(l2_block), u32::MAX));

        let create_call = DisputeGameFactory::createCall {
            _gameType: MOCK_PERMISSIONED_GAME_TYPE,
            _rootClaim: root_claim,
            _extraData: Bytes::from(extra_data.clone()),
        };
        env.send_factory_tx(create_call.abi_encode(), Some(init_bond)).await?;

        // Wait for the game to be indexed by the factory
        loop {
            let current = factory.gameCount().call().await?;
            if current > expected_index {
                expected_index = current;
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }

        let game_info = factory.gameAtIndex(expected_index - U256::from(1)).call().await?;
        let mock_game_address = game_info.proxy_;
        assert_eq!(game_info.gameType_, MOCK_PERMISSIONED_GAME_TYPE);
        info!(" • Mock permissioned game at {mock_game_address}");

        env.set_respected_game_type(TEST_GAME_TYPE).await?;

        let proposer_handle = env.start_proposer().await?;
        info!("✓ Proposer started after legacy games seeded");

        let tracked_games = env.wait_and_track_games(3, 30).await?;
        assert_eq!(tracked_games.len(), 3);
        info!("✓ Proposer created 3 type {} games despite legacy history", env.game_type);

        env.warp_time(MAX_CHALLENGE_DURATION).await?;

        let resolutions = env.wait_for_resolutions(&tracked_games, 30).await?;

        verify_all_resolved_correctly(&resolutions)?;

        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS).await?;

        env.wait_for_bond_claims(&tracked_games, PROPOSER_ADDRESS, 30).await?;

        env.stop_proposer(proposer_handle);

        let legacy_game =
            MockPermissionedDisputeGameView::new(mock_game_address, env.anvil.provider.clone());
        let status_raw = legacy_game.status().call().await?;
        let status = GameStatus::try_from(status_raw).with_context(|| {
            format!("Failed to decode mock permissioned game status for {mock_game_address}")
        })?;
        assert_eq!(
            status,
            GameStatus::IN_PROGRESS,
            "mock permissioned game should remain untouched"
        );

        Ok(())
    }

    // Ensures the proposer can handle a game type transition while running.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_type_transition_while_proposer_running() -> Result<()> {
        info!("=== Test: Game Type Transition While Proposer Running ===");

        let env = TestEnvironment::setup().await?;

        let factory_reader = env.factory()?;

        let init_bond = factory_reader.initBonds(TEST_GAME_TYPE).call().await?;

        let initial_game_count = factory_reader.gameCount().call().await?;

        // Setup legacy game type and set it as respected
        env.setup_legacy_game_type(MOCK_PERMISSIONED_GAME_TYPE, init_bond).await?;
        env.set_respected_game_type(MOCK_PERMISSIONED_GAME_TYPE).await?;

        let mut expected_index = initial_game_count;
        info!("Seeding legacy game (type {MOCK_PERMISSIONED_GAME_TYPE})");

        let proposer_handle = env.start_proposer().await?;

        let interval = 10;
        let l2_block = env.anvil.starting_l2_block_number + interval;
        let root_claim = env.compute_output_root_at_block(l2_block).await?;
        let extra_data = <(U256, u32)>::abi_encode_packed(&(U256::from(l2_block), u32::MAX));

        let create_call = DisputeGameFactory::createCall {
            _gameType: MOCK_PERMISSIONED_GAME_TYPE,
            _rootClaim: root_claim,
            _extraData: Bytes::from(extra_data.clone()),
        };
        env.send_factory_tx(create_call.abi_encode(), Some(init_bond)).await?;

        // Wait for the game to be indexed by the factory
        loop {
            let current = factory_reader.gameCount().call().await?;
            if current > expected_index {
                expected_index = current;
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }

        let game_info = factory_reader.gameAtIndex(expected_index - U256::from(1)).call().await?;
        let mock_game_address = game_info.proxy_;
        assert_eq!(game_info.gameType_, MOCK_PERMISSIONED_GAME_TYPE);
        info!(" • Mock permissioned game at {mock_game_address}");

        env.set_respected_game_type(TEST_GAME_TYPE).await?;

        let tracked_games = env.wait_and_track_games(3, 30).await?;
        assert_eq!(tracked_games.len(), 3);
        info!("✓ Proposer created 3 type {} games despite legacy history", env.game_type);

        env.warp_time(MAX_CHALLENGE_DURATION).await?;

        let resolutions = env.wait_for_resolutions(&tracked_games, 30).await?;

        verify_all_resolved_correctly(&resolutions)?;

        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS).await?;

        env.wait_for_bond_claims(&tracked_games, PROPOSER_ADDRESS, 30).await?;

        env.stop_proposer(proposer_handle);

        let legacy_game =
            MockPermissionedDisputeGameView::new(mock_game_address, env.anvil.provider.clone());
        let status_raw = legacy_game.status().call().await?;
        let status = GameStatus::try_from(status_raw).with_context(|| {
            format!("Failed to decode mock permissioned game status for {mock_game_address}")
        })?;
        assert_eq!(
            status,
            GameStatus::IN_PROGRESS,
            "mock permissioned game should remain untouched"
        );

        Ok(())
    }

    // Creates invalid output root games and runs the challenger to win and claim bonds.
    // Validates the challenger lifecycle handles creation, challenge, resolution, and payouts.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_honest_challenger_native() -> Result<()> {
        info!("=== Test: Honest Challenger Full Lifecycle (Challenge → Resolve → Claim) ===");

        const NUM_INVALID_GAMES: usize = 3;

        let env = TestEnvironment::setup().await?;

        info!("=== Starting Challenger Service ===");
        let challenger_handle = env.start_challenger(None).await?;

        // === PHASE 1: Create Invalid Games ===
        info!("=== Phase 1: Create Invalid Games ===");

        // Create a signer for permissioned account 0
        let factory = env.factory()?;

        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        let mut invalid_games = Vec::new();
        let mut rng = rand::rng();

        let mut l2_block_number = env.anvil.starting_l2_block_number;
        for _ in 0..NUM_INVALID_GAMES {
            l2_block_number += 10;
            // Create game with random invalid output root
            let mut invalid_root_bytes = [0u8; 32];
            rng.fill(&mut invalid_root_bytes);
            let invalid_root = FixedBytes::<32>::from(invalid_root_bytes);

            let _receipt =
                env.create_game(invalid_root, l2_block_number, u32::MAX, init_bond).await?;

            let (_, game_address) = env.last_game_info().await?;

            invalid_games.push(game_address);
        }

        info!("✓ Created {} invalid games:", invalid_games.len());
        for (i, game) in invalid_games.iter().enumerate() {
            info!("  Game {}: {}", i + 1, game);
        }

        // Verify challenger is still running
        assert!(!challenger_handle.is_finished(), "Challenger should still be running");
        info!("✓ Challenger is still running successfully");

        // === PHASE 2: Challenge Period ===
        info!("=== Phase 2: Challenge Period ===");
        env.wait_for_challenges(&invalid_games, 30).await?;
        info!("✓ All games challenged successfully");

        // === PHASE 3: Resolution ===
        info!("=== Phase 3: Resolution ===");
        env.warp_time(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION).await?;
        info!(
            "✓ Warped time past prove deadline (challenge {}s + prove {}s) to trigger challenger wins",
            MAX_CHALLENGE_DURATION,
            MAX_PROVE_DURATION
        );

        // Wait for and verify challenger wins
        env.wait_and_verify_game_resolutions(
            &invalid_games,
            GameStatus::CHALLENGER_WINS,
            "ChallengerWins",
            30,
        )
        .await?;

        // Warp DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1 for bond claims
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1).await?;
        info!(
            "✓ Warped time to enable bond claims ({} seconds)",
            DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1
        );

        // Verify challenger is still running
        assert!(!challenger_handle.is_finished(), "Challenger should still be running");
        info!("✓ Challenger is still running successfully");

        // === PHASE 4: Bond Claims ===
        info!("=== Phase 4: Bond Claims ===");

        // Wait for challenger to claim bonds
        let tracked_games: Vec<_> = invalid_games
            .iter()
            .map(|&address| TrackedGame {
                index: U256::ZERO, // Not needed for bond claim check
                address,
                parent_index: u32::MAX,      // Not needed for bond claim check
                l2_block_number: U256::ZERO, // Not needed for bond claim check
            })
            .collect();

        env.wait_for_bond_claims(&tracked_games, CHALLENGER_ADDRESS, 30).await?;

        // Stop challenger
        info!("=== Stopping Challenger ===");
        env.stop_challenger(challenger_handle);

        info!("=== Full Lifecycle Test Complete ===");
        info!("✓ Invalid games challenged, won by challenger, and bonds claimed successfully");

        Ok(())
    }

    // Builds a chain with an invalid parent root and ensures the proposer rejects it.
    // Demonstrates new games anchor on valid history instead of corrupted branches.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_chain_validation_invalid_parent() -> Result<()> {
        info!("=== Test: Game Chain Validation - Invalid Parent Chain ===");

        let env = TestEnvironment::setup().await?;

        let factory = env.factory()?;

        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        info!("Advancing time to ensure games won't be retired...");
        env.warp_time(10).await?;

        // === PHASE 1: Create Invalid Parent Chain ===
        info!("=== Phase 1: Creating Invalid Parent Chain ===");

        // Step 1: Create a valid anchor game (parentIndex = u32::MAX)
        let anchor_block = env.anvil.starting_l2_block_number + 1;

        // Create L2 provider to compute output roots
        let anchor_root = env.compute_output_root_at_block(anchor_block).await?;

        env.create_game(anchor_root, anchor_block, u32::MAX, init_bond).await?;

        let (anchor_game_index, anchor_game_address) = env.last_game_info().await?;

        info!(
            "✓ Created valid anchor game at index {} (address: {})",
            anchor_game_index, anchor_game_address
        );

        // Step 2: Create an invalid middle game with wrong output root
        let middle_block = anchor_block + 10;
        let mut rng = rand::rng();
        let mut invalid_root_bytes = [0u8; 32];
        rng.fill(&mut invalid_root_bytes);
        let invalid_root = FixedBytes::<32>::from(invalid_root_bytes);

        env.create_game(invalid_root, middle_block, anchor_game_index.to::<u32>(), init_bond)
            .await?;

        let (middle_game_index, middle_game_address) = env.last_game_info().await?;
        info!(
            "✓ Created invalid middle game at index {} (address: {})",
            middle_game_index, middle_game_address
        );

        // Step 3: Create a valid child game pointing to invalid parent
        let child_block = middle_block + 10;
        let child_root = env.compute_output_root_at_block(child_block).await?;
        env.create_game(child_root, child_block, middle_game_index.to::<u32>(), init_bond).await?;

        let (child_game_index, child_game_address) = env.last_game_info().await?;
        info!(
            "✓ Created valid child game at index {} (address: {})",
            child_game_index, child_game_address
        );

        // === PHASE 2: Start Proposer and Verify Chain Rejection ===
        info!("=== Phase 2: Starting Proposer to Validate Chain ===");

        let proposer_handle = env.start_proposer().await?;

        // Wait for proposer to create a new game (it should skip the invalid chain)
        let initial_game_count = child_game_index + U256::from(1);
        let mut new_game_created = false;

        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let current_game_count = factory.gameCount().call().await?;

            if current_game_count > initial_game_count {
                new_game_created = true;
                // Check the FIRST game created by the proposer (at index initial_game_count)
                let new_game_index = initial_game_count; // First new game is at this index
                let new_game_info = factory.gameAtIndex(new_game_index).call().await?;

                // Check that the new game doesn't build on the invalid chain
                let new_game = env.fault_dispute_game(new_game_info.proxy_).await?;

                let claim_data = new_game.claimData().call().await?;

                // The new game should either be an anchor game or build on the valid anchor game
                assert!(
                    claim_data.parentIndex == u32::MAX ||
                        U256::from(claim_data.parentIndex) <= anchor_game_index,
                    "Proposer should not build on invalid chain"
                );

                info!(
                    "✓ Proposer correctly skipped invalid chain and created new game at index {}",
                    new_game_index
                );
                info!("  New game parent index: {}", claim_data.parentIndex);
                break;
            }
        }

        assert!(new_game_created, "Proposer should have created a new game");

        env.stop_proposer(proposer_handle);
        info!("✓ Test complete: Proposer correctly rejected invalid parent chain");

        Ok(())
    }

    // Challenges the parent after a child is created and checks proposer behavior.
    // Confirms the chain with a challenged ancestor is abandoned for a fresh anchor.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_chain_validation_challenged_parent() -> Result<()> {
        info!("=== Test: Game Chain Validation - Challenged Parent ===");

        let env = TestEnvironment::setup().await?;

        let factory = env.factory()?;

        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        // CRITICAL FIX: Advance time after contract deployment
        // This ensures games created won't be considered "retired" (createdAt >
        // respectedGameTypeUpdatedAt)
        info!("Advancing time to ensure games won't be retired...");
        env.warp_time(10).await?;

        // === PHASE 1: Create Valid Parent Game ===
        info!("=== Phase 1: Creating Valid Parent Game ===");

        // Create a valid parent game that will be challenged
        let parent_block = env.anvil.starting_l2_block_number + 1;

        let parent_root = env.compute_output_root_at_block(parent_block).await?;

        let receipt = env.create_game(parent_root, parent_block, u32::MAX, init_bond).await?;
        info!("Parent game creation tx confirmed in block {:?}", receipt.block_number);

        let (parent_game_index, parent_game_address) = env.last_game_info().await?;
        info!(
            "✓ Created valid parent game at index {} (address: {})",
            parent_game_index, parent_game_address
        );

        // === PHASE 2: Create Child Game Referencing Valid Parent ===
        info!("=== Phase 2: Creating Child Game with Valid Parent ===");

        // Double-check parent game status right before creating child
        let parent_game = env.fault_dispute_game(parent_game_address).await?;

        let parent_status = parent_game.status().call().await?;
        info!("Parent game status right before child creation: {:?}", parent_status);

        let was_respected = parent_game.wasRespectedGameTypeWhenCreated().call().await?;
        info!("Parent game wasRespectedGameTypeWhenCreated: {}", was_respected);

        // Check if parent game is blacklisted or retired via AnchorStateRegistry
        let anchor_registry = env.anchor_registry(parent_game_address).await?;

        let is_respected = anchor_registry.isGameRespected(parent_game_address).call().await?;
        let is_blacklisted = anchor_registry.isGameBlacklisted(parent_game_address).call().await?;
        let is_retired = anchor_registry.isGameRetired(parent_game_address).call().await?;

        info!("Parent game validation via AnchorStateRegistry:");
        info!("  - isGameRespected: {}", is_respected);
        info!("  - isGameBlacklisted: {}", is_blacklisted);
        info!("  - isGameRetired: {}", is_retired);

        // Debug timing issue - why is game retired?
        if is_retired {
            let parent_created_at = parent_game.createdAt().call().await?;

            let portal = env.mock_optimism_portal2(anchor_registry).await?;

            let respected_game_type_updated_at = portal.respectedGameTypeUpdatedAt().call().await?;

            info!("DEBUG: Game retirement issue:");
            info!("  - Parent game createdAt: {}", parent_created_at);
            info!("  - Portal respectedGameTypeUpdatedAt: {}", respected_game_type_updated_at);
            info!(
                "  - Game is retired because: {} <= {}",
                parent_created_at, respected_game_type_updated_at
            );
        }

        let child_block = parent_block + 10;
        let child_root = env.compute_output_root_at_block(child_block).await?;

        info!(
            "Attempting to create child game with parent index: {}",
            parent_game_index.to::<u32>()
        );
        let receipt = env
            .create_game(child_root, child_block, parent_game_index.to::<u32>(), init_bond)
            .await?;
        info!("Child game creation tx confirmed in block {:?}", receipt.block_number);

        let (child_game_index, child_game_address) = env.last_game_info().await?;
        info!(
            "✓ Created child game at index {} (address: {})",
            child_game_index, child_game_address
        );

        // === PHASE 3: Challenge and Resolve Parent as CHALLENGER_WINS ===
        info!("=== Phase 3: Challenging Parent Game (after child was created) ===");

        // Start challenger to challenge the game (using malicious mode to challenge valid game)
        let challenger_handle = env.start_challenger(Some(100.0)).await?;

        // Wait for challenge
        env.wait_for_challenges(&[parent_game_address], 30).await?;
        info!("✓ Parent game challenged");

        // Warp time to resolve as CHALLENGER_WINS (no proof submitted)
        env.warp_time(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION).await?;

        // Wait for resolution
        env.wait_and_verify_game_resolutions(
            &[parent_game_address],
            GameStatus::CHALLENGER_WINS,
            "ChallengerWins",
            30,
        )
        .await?;
        info!("✓ Parent game resolved as CHALLENGER_WINS");

        env.stop_challenger(challenger_handle);
        // === PHASE 4: Start Proposer and Verify Chain Rejection ===
        info!("=== Phase 4: Starting Proposer to Validate Chain ===");

        // Start proposer
        let proposer_handle = env.start_proposer().await?;

        // Wait for proposer to create a new game (it should skip the chain with challenged parent)
        let initial_game_count = child_game_index + U256::from(1);
        let mut new_game_created = false;

        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let current_game_count = factory.gameCount().call().await?;

            if current_game_count > initial_game_count {
                new_game_created = true;
                let new_game_index = current_game_count - U256::from(1);
                let new_game_info = factory.gameAtIndex(new_game_index).call().await?;

                // Check that the new game doesn't build on the challenged parent chain
                let new_game = env.fault_dispute_game(new_game_info.proxy_).await?;
                let claim_data = new_game.claimData().call().await?;

                // The new game should be a new anchor game (parentIndex = u32::MAX)
                // since the entire chain is invalid due to challenged parent
                assert_eq!(
                claim_data.parentIndex,
                u32::MAX,
                "Proposer should create a new anchor game when all chains have challenged ancestors"
            );

                info!("✓ Proposer correctly skipped chain with challenged parent and created new anchor game at index {}", new_game_index);
                info!("  New game parent index: {} (anchor game)", claim_data.parentIndex);
                break;
            }
        }

        assert!(new_game_created, "Proposer should have created a new game");

        env.stop_proposer(proposer_handle);
        info!("✓ Test complete: Proposer correctly rejected chain with challenged parent");

        Ok(())
    }

    // Resets the anchor to a new finalized chain and monitors proposer syncing.
    // Verifies new games follow the updated anchor branch after reset events.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_chain_validation_anchor_reset() -> Result<()> {
        info!("=== Test: Game Chain Validation - Anchor Reset ===");

        let env = TestEnvironment::setup().await?;

        let factory = env.factory()?;

        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        // Prevent games from being retired immediately after deployment
        info!("Advancing time to ensure games won't be retired...");
        env.warp_time(10).await?;

        // === PHASE 1: Create initial canonical chain (A0 -> A1) ===
        info!("=== Phase 1: Creating Initial Canonical Chain ===");

        let a0_block = env.anvil.starting_l2_block_number + 1;
        let a0_root = env.compute_output_root_at_block(a0_block).await?;
        let receipt = env.create_game(a0_root, a0_block, u32::MAX, init_bond).await?;
        info!("Game A0 creation tx confirmed in block {:?}", receipt.block_number);

        let (a0_index, a0_address) = env.last_game_info().await?;
        info!("✓ Created game A0 at index {} (address: {})", a0_index, a0_address);

        let a1_block = a0_block + 10;
        let a1_root = env.compute_output_root_at_block(a1_block).await?;
        let receipt = env.create_game(a1_root, a1_block, u32::MAX, init_bond).await?;
        info!("Game A1 creation tx confirmed in block {:?}", receipt.block_number);

        let (a1_index, a1_address) = env.last_game_info().await?;
        info!("✓ Created game A1 at index {} (address: {})", a1_index, a1_address);

        // Record current anchor game
        let anchor_registry = env.anchor_registry(a0_address).await?;
        let initial_anchor = anchor_registry.anchorGame().call().await?;
        info!("Initial anchor game: {}", initial_anchor);

        // === PHASE 2: Create new anchor chain (B0 -> B1) ===
        info!("=== Phase 2: Creating Alternate Anchor Chain ===");

        let b0_block = a1_block + 10;
        let b0_root = env.compute_output_root_at_block(b0_block).await?;
        let receipt = env.create_game(b0_root, b0_block, u32::MAX, init_bond).await?;
        info!("Game B0 creation tx confirmed in block {:?}", receipt.block_number);

        let (b0_index, b0_address) = env.last_game_info().await?;
        info!("✓ Created game B0 at index {} (address: {})", b0_index, b0_address);

        let b1_block = b0_block + 10;
        let b1_root = env.compute_output_root_at_block(b1_block).await?;
        let receipt = env.create_game(b1_root, b1_block, b0_index.to::<u32>(), init_bond).await?;
        info!("Game B1 creation tx confirmed in block {:?}", receipt.block_number);

        let (b1_index, b1_address) = env.last_game_info().await?;
        info!("✓ Created game B1 at index {} (address: {})", b1_index, b1_address);

        // Resolve the new chain so it becomes eligible as the anchor
        let finality_wait =
            DISPUTE_GAME_FINALITY_DELAY_SECONDS + MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION + 60;
        info!("Warping time by {} seconds to finalize new chain", finality_wait);
        env.warp_time(finality_wait).await?;

        let receipt = env.resolve_game(b0_address).await?;
        info!("Game B0 resolved in tx {:?}", receipt.transaction_hash);

        let receipt = env.resolve_game(b1_address).await?;
        info!("Game B1 resolved in tx {:?}", receipt.transaction_hash);

        // Advance beyond finality delay so the new chain is finalized
        info!(
            "Warping time by {} seconds to finalize resolved games",
            DISPUTE_GAME_FINALITY_DELAY_SECONDS + 60
        );
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 60).await?;

        // Force anchor to update to the new chain rooted at B0
        let receipt = env.set_anchor_state(b0_address).await?;
        info!("Anchor reset tx confirmed in block {:?}", receipt.block_number);

        // Ensure anchor switched to new chain
        let updated_anchor = anchor_registry.anchorGame().call().await?;
        info!("Updated anchor game: {}", updated_anchor);
        assert_ne!(
            updated_anchor, initial_anchor,
            "Anchor should update to the new branch before starting proposer"
        );

        // === PHASE 3: Start proposer and verify it follows new anchor chain ===
        info!("=== Phase 3: Starting Proposer to Observe Anchor Reset ===");

        let proposer_handle = env.start_proposer().await?;
        info!("✓ Proposer service started");

        let mut new_game_parent_verified = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let current_count = factory.gameCount().call().await?;

            if current_count > b1_index + U256::from(1) {
                let new_game_index = current_count - U256::from(1);
                let new_game_info = factory.gameAtIndex(new_game_index).call().await?;
                let new_game = env.fault_dispute_game(new_game_info.proxy_).await?;
                let claim_data = new_game.claimData().call().await?;

                assert_eq!(
                    claim_data.parentIndex,
                    b1_index.to::<u32>(),
                    "Proposer should build on the new anchor branch after reset"
                );

                info!(
                    "✓ Proposer correctly followed anchor reset and created game {} with parent {}",
                    new_game_index, claim_data.parentIndex
                );
                new_game_parent_verified = true;
                break;
            }
        }

        assert!(
            new_game_parent_verified,
            "Proposer should have created a game on the new anchor branch"
        );

        proposer_handle.abort();
        info!("✓ Test complete: Proposer handled anchor reset correctly");

        Ok(())
    }

    // Tests proposer recovery when its canonical head is invalidated during runtime.
    // When the canonical head is invalidated, the proposer should branch off from the latest valid
    // game.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_proposer_recovery_after_canonical_head_invalidation() -> Result<()> {
        info!("=== Test: Proposer Recovery After Runtime Canonical Head Invalidation ===");

        let env = TestEnvironment::setup().await?;

        // === PHASE 1: Start Proposer and Create Initial Games ===
        info!("=== Phase 1: Start Proposer and Create Games ===");

        let proposer_handle = env.start_proposer().await?;

        // Wait for proposer to create 3 games
        let factory = env.factory()?;

        let tracked_games = env.wait_and_track_games(3, 30).await?;
        info!("✓ Proposer created {} games:", tracked_games.len());

        assert!(!proposer_handle.is_finished(), "Proposer should be running");

        // === PHASE 2: Challenge Game 3 =================================
        info!("=== Phase 2: Challenge Game 3 ===");
        let challenger = init_challenger(
            &env.rpc_config,
            env.private_keys.challenger,
            &env.deployed.factory,
            env.game_type,
            Some(100.0),
        )
        .await?;
        info!("✓ Challenger initialized");

        let game_to_challenge = Game {
            index: U256::from(2),
            address: tracked_games[2].address,
            parent_index: 1,
            l2_block_number: tracked_games[2].l2_block_number,
            is_invalid: false,
            status: GameStatus::IN_PROGRESS,
            proposal_status: ProposalStatus::Unchallenged,
            should_attempt_to_challenge: true,
            should_attempt_to_resolve: false,
            should_attempt_to_claim_bond: false,
        };
        challenger.submit_challenge_transaction(&game_to_challenge).await?;
        info!("✓ Challenged game 3");

        // === PHASE 3: Resolve all 3 games and finalize the first 2 games ===
        info!("=== Phase 3: Resolve all 3 games ===");
        info!("This should clear game 3 from the proposer's cache");

        // Warp time to allow first 2 unchallenged games to be resolved
        env.warp_time(MAX_CHALLENGE_DURATION).await?;
        info!("✓ Warped time by MAX_CHALLENGE_DURATION to enable resolution for first 2 games");

        let first_two_games = &tracked_games[0..2];

        let resolutions = env.wait_for_resolutions(first_two_games, 30).await?;
        verify_all_resolved_correctly(&resolutions)?;
        info!("✓ First 2 games resolved as DEFENDER_WINS");

        // Warp time to allow challenged game 3 to be resolved as CHALLENGER_WINS
        env.warp_time(MAX_PROVE_DURATION).await?;
        challenger.submit_resolution_transaction(&game_to_challenge).await?;
        info!("✓ Challenger resolved game 3 as CHALLENGER_WINS");

        assert!(!proposer_handle.is_finished(), "Proposer should still be running");

        // Warp time to allow the proposer to finalize the first 2 games
        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS).await?;

        env.wait_for_bond_claims(first_two_games, PROPOSER_ADDRESS, 30).await?;
        info!("✓ Proposer finalized the first 2 games");

        // === PHASE 4: Verify Proposer recovers automatically ===
        info!("=== Phase 4: Verify Proposer recovers automatically ===");
        info!("Proposer should create a new game from the last valid game at index 1");

        let mut current_game_count = factory.gameCount().call().await?;
        info!("Current game count: {}", current_game_count);

        const FIRST_NEW_GAME_INDEX: u64 = 3;
        const EXPECTED_PARENT_INDEX: u32 = 1;

        // Wait for proposer to create a new game beyond the invalidated canonical head
        let mut i = U256::from(FIRST_NEW_GAME_INDEX);
        while current_game_count <= i {
            tokio::time::sleep(Duration::from_secs(1)).await;
            current_game_count = factory.gameCount().call().await?;
        }

        // Check newly created games to find one that builds on the last valid game
        let mut found = false;
        while i < current_game_count {
            // Verify the new game is built on the last valid game at index 1
            let new_game_info = factory.gameAtIndex(i).call().await?;
            let new_game = env.fault_dispute_game(new_game_info.proxy_).await?;
            let claim_data = new_game.claimData().call().await?;
            if claim_data.parentIndex == EXPECTED_PARENT_INDEX {
                found = true;
                break;
            }
            i += U256::from(1);
        }
        assert!(found, "Proposer should create a new game from the last valid game");

        // Verify proposer continues to operate normally
        assert!(!proposer_handle.is_finished(), "Proposer should continue running after recovery");

        // Stop proposer
        proposer_handle.abort();
        info!("✓ Proposer stopped");

        Ok(())
    }

    // Covers the proposer retaining the anchor game after bond claims.
    // This test verifies that the proposer correctly identifies and retains
    // the anchor game in its cache even after claiming bonds for finalized games.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_proposer_retains_anchor_after_bond_claim() -> Result<()> {
        let env = TestEnvironment::setup().await?;

        let proposer = Arc::new(env.init_proposer().await?);

        let proposer_handle = {
            let proposer_clone = proposer.clone();
            tokio::spawn(async move { proposer_clone.run().await })
        };

        let tracked_games = env.wait_and_track_games(3, 30).await?;

        env.warp_time(MAX_CHALLENGE_DURATION).await?;

        let resolutions = env.wait_for_resolutions(&tracked_games, 30).await?;
        verify_all_resolved_correctly(&resolutions)?;

        env.warp_time(DISPUTE_GAME_FINALITY_DELAY_SECONDS).await?;

        env.wait_for_bond_claims(&tracked_games, PROPOSER_ADDRESS, 30).await?;

        // Allow the proposer loop to observe the finalized games and update its cache.
        let settle_delay = Duration::from_secs(proposer.config.fetch_interval + 5);
        sleep(settle_delay).await;

        let snapshot = proposer.state_snapshot().await;

        // The third game with index 2 should be the anchor game.
        let expected_anchor_index = U256::from(2);
        assert_eq!(snapshot.anchor_index, Some(expected_anchor_index));

        // Verify the anchor game is present in the games cache.
        let _snapshot_anchor_game = snapshot
            .games
            .iter()
            .find(|(index, _)| *index == expected_anchor_index)
            .expect("anchor game not found in snapshot");

        env.stop_proposer(proposer_handle);

        Ok(())
    }
}
