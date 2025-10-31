pub mod common;

#[cfg(feature = "e2e")]
mod e2e {
    use super::*;
    use std::str::FromStr;

    use alloy_network::EthereumWallet;
    use alloy_primitives::{Bytes, FixedBytes, U256};
    use alloy_provider::ProviderBuilder;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_sol_types::{SolCall, SolValue};
    use alloy_transport_http::reqwest::Url;
    use anyhow::{Context, Result};
    use common::{
        constants::{
            CHALLENGER_ADDRESS, CHALLENGER_PRIVATE_KEY, DISPUTE_GAME_FINALITY_DELAY_SECONDS,
            MAX_CHALLENGE_DURATION, MAX_PROVE_DURATION, MOCK_PERMISSIONED_GAMES_TO_SEED,
            MOCK_PERMISSIONED_GAME_TYPE, PROPOSER_ADDRESS, PROPOSER_PRIVATE_KEY, TEST_GAME_TYPE,
        },
        contracts::{deploy_mock_permissioned_game, send_contract_transaction},
        monitor::{
            verify_all_resolved_correctly, wait_and_track_games, wait_and_verify_game_resolutions,
            wait_for_bond_claims, wait_for_challenges, wait_for_resolutions, TrackedGame,
        },
        warp_time, TestEnvironment,
    };
    use fault_proof::{
        challenger::Game,
        contract::{GameStatus, ProposalStatus},
        proposer::Game as ProposerGame,
        L2ProviderTrait,
    };
    use op_succinct_bindings::{
        dispute_game_factory::DisputeGameFactory, mock_optimism_portal2::MockOptimismPortal2,
    };
    use op_succinct_signer_utils::{Signer, SignerLock};
    use rand::Rng;
    use tokio::time::{sleep, Duration};
    use tracing::info;

    use crate::common::{init_challenger, init_proposer, start_challenger, start_proposer};

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
        TestEnvironment::init_logging();
        info!("=== Test: Honest Proposer Full Lifecycle (Create → Resolve → Claim) ===");

        // Setup common test environment
        let env = TestEnvironment::setup().await?;

        // Start proposer
        let proposer_handle = start_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;
        info!("✓ Proposer service started");

        // Wait for proposer to create games
        info!("=== Waiting for Game Creation ===");
        let factory = DisputeGameFactory::new(env.deployed.factory, env.anvil.provider.clone());

        // Track first 3 games (L2 finalized head won't advance far enough for 3)
        let tracked_games =
            wait_and_track_games(&factory, TEST_GAME_TYPE, 3, Duration::from_secs(120)).await?;

        info!("✓ Proposer created {} games:", tracked_games.len());
        for (i, game) in tracked_games.iter().enumerate() {
            info!("  Game {}: {} at L2 block {}", i + 1, game.address, game.l2_block_number);
        }

        // Verify proposer is still running
        assert!(!proposer_handle.is_finished(), "Proposer should still be running");
        info!("✓ Proposer is still running successfully");

        // === PHASE 2: Challenge Period ===
        info!("=== Phase 2: Challenge Period ===");
        info!("Warping time to near end of max challenge duration...");

        // Warp by max challenge duration
        warp_time(&env.anvil.provider, Duration::from_secs(MAX_CHALLENGE_DURATION)).await?;
        info!("✓ Warped time by max challenge duration ({MAX_CHALLENGE_DURATION} seconds) to trigger resolution");

        // Verify proposer is still running
        assert!(!proposer_handle.is_finished(), "Proposer should still be running");
        info!("✓ Proposer is still running successfully");

        // === PHASE 3: Resolution ===
        info!("=== Phase 3: Resolution ===");

        // Wait for games to be resolved
        let resolutions =
            wait_for_resolutions(&env.anvil.provider, &tracked_games, Duration::from_secs(120))
                .await?;

        // Verify all games resolved correctly (proposer wins)
        verify_all_resolved_correctly(&resolutions)?;

        // Warp past DISPUTE_GAME_FINALITY_DELAY_SECONDS
        warp_time(&env.anvil.provider, Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS))
            .await?;
        info!("✓ Warped time by DISPUTE_GAME_FINALITY_DELAY_SECONDS ({DISPUTE_GAME_FINALITY_DELAY_SECONDS} seconds) to trigger bond claims");

        // Verify proposer is still running
        assert!(!proposer_handle.is_finished(), "Proposer should still be running");
        info!("✓ Proposer is still running successfully");

        // === PHASE 4: Bond Claims ===
        info!("=== Phase 4: Bond Claims ===");

        // Wait for proposer to claim bonds
        wait_for_bond_claims(
            &env.anvil.provider,
            &tracked_games,
            PROPOSER_ADDRESS,
            Duration::from_secs(120),
        )
        .await?;

        // Stop proposer
        info!("=== Stopping Proposer ===");
        proposer_handle.abort();
        info!("✓ Proposer stopped gracefully");

        info!("=== Full Lifecycle Test Complete ===");
        info!("✓ Games created, resolved, and bonds claimed successfully");

        Ok(())
    }

    // Seeds legacy games, transitions game types, and verifies the proposer ignores them.
    // Confirms new games are created, resolved, and bonds claimed despite history.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_type_transition_skips_legacy_games() -> Result<()> {
        TestEnvironment::init_logging();
        info!("=== Test: Game Type Transition With Legacy Games In History ===");

        let env = TestEnvironment::setup().await?;
        let mut rng = rand::rng();

        let l1_rpc_url = env.rpc_config.l1_rpc.clone();
        let factory_reader =
            DisputeGameFactory::new(env.deployed.factory, env.anvil.provider.clone());
        let initial_game_count = factory_reader.gameCount().call().await?;
        let init_bond = factory_reader.initBonds(TEST_GAME_TYPE).call().await?;

        let proposer_signer = SignerLock::new(Signer::new_local_signer(PROPOSER_PRIVATE_KEY)?);

        let legacy_impl = deploy_mock_permissioned_game(&proposer_signer, &l1_rpc_url).await?;
        info!("✓ Deployed mock permissioned implementation at {legacy_impl}");

        let set_init_call = DisputeGameFactory::setInitBondCall {
            _gameType: MOCK_PERMISSIONED_GAME_TYPE,
            _initBond: init_bond,
        };
        send_contract_transaction(
            &proposer_signer,
            &l1_rpc_url,
            env.deployed.factory,
            Bytes::from(set_init_call.abi_encode()),
            None,
        )
        .await?;

        let set_impl_call = DisputeGameFactory::setImplementationCall {
            _gameType: MOCK_PERMISSIONED_GAME_TYPE,
            _impl: legacy_impl,
        };
        send_contract_transaction(
            &proposer_signer,
            &l1_rpc_url,
            env.deployed.factory,
            Bytes::from(set_impl_call.abi_encode()),
            None,
        )
        .await?;

        let legacy_game_type_call = MockOptimismPortal2::setRespectedGameTypeCall {
            _gameType: MOCK_PERMISSIONED_GAME_TYPE,
        };
        send_contract_transaction(
            &proposer_signer,
            &l1_rpc_url,
            env.deployed.portal,
            Bytes::from(legacy_game_type_call.abi_encode()),
            None,
        )
        .await?;

        let mut expected_index = initial_game_count;
        let mut mock_game_addresses = Vec::with_capacity(MOCK_PERMISSIONED_GAMES_TO_SEED);

        info!(
        "Seeding {MOCK_PERMISSIONED_GAMES_TO_SEED} legacy games (type {MOCK_PERMISSIONED_GAME_TYPE})"
    );
        for i in 0..MOCK_PERMISSIONED_GAMES_TO_SEED {
            let mut root_bytes = [0u8; 32];
            rng.fill(&mut root_bytes);
            let root_claim = FixedBytes::<32>::from(root_bytes);
            let l2_block = U256::from(env.anvil.starting_l2_block_number + ((i as u64 + 1) * 5));
            let extra_data = <(U256, u32)>::abi_encode_packed(&(l2_block, u32::MAX));

            let create_call = DisputeGameFactory::createCall {
                _gameType: MOCK_PERMISSIONED_GAME_TYPE,
                _rootClaim: root_claim,
                _extraData: Bytes::from(extra_data.clone()),
            };

            send_contract_transaction(
                &proposer_signer,
                &l1_rpc_url,
                env.deployed.factory,
                Bytes::from(create_call.abi_encode()),
                Some(init_bond),
            )
            .await?;

            // Wait for the game to be indexed by the factory
            loop {
                let current = factory_reader.gameCount().call().await?;
                if current > expected_index {
                    expected_index = current;
                    break;
                }
                sleep(Duration::from_millis(200)).await;
            }

            let game_info =
                factory_reader.gameAtIndex(expected_index - U256::from(1)).call().await?;
            assert_eq!(game_info.gameType_, MOCK_PERMISSIONED_GAME_TYPE);
            mock_game_addresses.push(game_info.proxy_);
            info!(
                "  • Mock permissioned game {}/{} at {}",
                i + 1,
                MOCK_PERMISSIONED_GAMES_TO_SEED,
                game_info.proxy_
            );
        }

        let restore_type_call =
            MockOptimismPortal2::setRespectedGameTypeCall { _gameType: TEST_GAME_TYPE };
        send_contract_transaction(
            &proposer_signer,
            &l1_rpc_url,
            env.deployed.portal,
            Bytes::from(restore_type_call.abi_encode()),
            None,
        )
        .await?;

        let proposer_handle = start_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;
        info!("✓ Proposer started after legacy games seeded");

        let factory = DisputeGameFactory::new(env.deployed.factory, env.anvil.provider.clone());
        let tracked_games =
            wait_and_track_games(&factory, TEST_GAME_TYPE, 3, Duration::from_secs(120)).await?;
        assert_eq!(tracked_games.len(), 3);
        info!("✓ Proposer created 3 type {} games despite legacy history", TEST_GAME_TYPE);

        warp_time(&env.anvil.provider, Duration::from_secs(MAX_CHALLENGE_DURATION)).await?;
        let resolutions =
            wait_for_resolutions(&env.anvil.provider, &tracked_games, Duration::from_secs(120))
                .await?;
        verify_all_resolved_correctly(&resolutions)?;

        warp_time(&env.anvil.provider, Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS))
            .await?;
        wait_for_bond_claims(
            &env.anvil.provider,
            &tracked_games,
            PROPOSER_ADDRESS,
            Duration::from_secs(120),
        )
        .await?;

        proposer_handle.abort();

        for (idx, address) in mock_game_addresses.iter().enumerate() {
            let legacy_game =
                MockPermissionedDisputeGameView::new(*address, env.anvil.provider.clone());
            let status_raw = legacy_game.status().call().await?;
            let status = GameStatus::try_from(status_raw).with_context(|| {
                format!("Failed to decode mock permissioned game status for {address}")
            })?;
            assert_eq!(
                status,
                GameStatus::IN_PROGRESS,
                "mock permissioned game {idx} should remain untouched"
            );
        }

        Ok(())
    }

    // Creates invalid output root games and runs the challenger to win and claim bonds.
    // Validates the challenger lifecycle handles creation, challenge, resolution, and payouts.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_honest_challenger_native() -> Result<()> {
        TestEnvironment::init_logging();
        info!("=== Test: Honest Challenger Full Lifecycle (Challenge → Resolve → Claim) ===");

        const NUM_INVALID_GAMES: usize = 3;

        // Setup common test environment
        let env = TestEnvironment::setup().await?;
        let mut l2_block_number = env.anvil.starting_l2_block_number;

        // Start challenger service
        info!("=== Starting Challenger Service ===");
        let challenger_handle = start_challenger(
            &env.rpc_config,
            CHALLENGER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
            None,
        )
        .await?;
        info!("✓ Challenger service started");

        // === PHASE 1: Create Invalid Games ===
        info!("=== Phase 1: Create Invalid Games ===");

        // Create a signer for permissioned account 0
        let wallet = PrivateKeySigner::from_str(PROPOSER_PRIVATE_KEY)?;
        let provider_with_signer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet))
            .connect_http(env.anvil.endpoint.parse::<Url>()?);

        let factory = DisputeGameFactory::new(env.deployed.factory, provider_with_signer.clone());
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        let mut invalid_games = Vec::new();
        let mut rng = rand::rng();

        for _ in 0..NUM_INVALID_GAMES {
            l2_block_number += 10;
            // Create game with random invalid output root
            let mut invalid_root_bytes = [0u8; 32];
            rng.fill(&mut invalid_root_bytes);
            let invalid_root = FixedBytes::<32>::from(invalid_root_bytes);

            let parent_index = u32::MAX;
            let extra_data = (U256::from(l2_block_number), parent_index).abi_encode_packed();

            let tx = factory
                .create(TEST_GAME_TYPE, invalid_root, extra_data.into())
                .value(init_bond)
                .send()
                .await?;

            let _receipt = tx.get_receipt().await?;
            let new_game_count = factory.gameCount().call().await?;
            let game_index = new_game_count - U256::from(1);
            let game_info = factory.gameAtIndex(game_index).call().await?;
            let game_address = game_info.proxy_;

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
        wait_for_challenges(&env.anvil.provider, &invalid_games, Duration::from_secs(60)).await?;
        info!("✓ All games challenged successfully");

        // === PHASE 3: Resolution ===
        info!("=== Phase 3: Resolution ===");
        info!("Warping time past prove deadline to trigger challenger wins...");
        warp_time(
            &env.anvil.provider,
            Duration::from_secs(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION),
        )
        .await?;
        info!(
            "✓ Warped time by {} seconds (max challenge duration + max prove duration)",
            MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION
        );

        // Wait for and verify challenger wins
        wait_and_verify_game_resolutions(
            &env.anvil.provider,
            &invalid_games,
            GameStatus::CHALLENGER_WINS,
            "ChallengerWins",
            Duration::from_secs(30),
        )
        .await?;

        // Warp DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1 for bond claims
        warp_time(
            &env.anvil.provider,
            Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1),
        )
        .await?;
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

        wait_for_bond_claims(
            &env.anvil.provider,
            &tracked_games,
            CHALLENGER_ADDRESS,
            Duration::from_secs(60),
        )
        .await?;

        // Stop challenger
        info!("=== Stopping Challenger ===");
        challenger_handle.abort();
        info!("✓ Challenger stopped gracefully");

        info!("=== Full Lifecycle Test Complete ===");
        info!("✓ Invalid games challenged, won by challenger, and bonds claimed successfully");

        Ok(())
    }

    // Builds a chain with an invalid parent root and ensures the proposer rejects it.
    // Demonstrates new games anchor on valid history instead of corrupted branches.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_chain_validation_invalid_parent() -> Result<()> {
        TestEnvironment::init_logging();
        info!("=== Test: Game Chain Validation - Invalid Parent Chain ===");

        // Setup common test environment
        let env = TestEnvironment::setup().await?;

        // Create a signer for creating games
        let wallet = PrivateKeySigner::from_str(PROPOSER_PRIVATE_KEY)?;
        let provider_with_signer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet))
            .connect_http(env.anvil.endpoint.parse::<Url>()?);

        let factory = DisputeGameFactory::new(env.deployed.factory, provider_with_signer.clone());
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        info!("Advancing time to ensure games won't be retired...");
        warp_time(&env.anvil.provider, Duration::from_secs(10)).await?;

        // === PHASE 1: Create Invalid Parent Chain ===
        info!("=== Phase 1: Creating Invalid Parent Chain ===");

        // Step 1: Create a valid anchor game (parentIndex = u32::MAX)
        let anchor_block = env.anvil.starting_l2_block_number + 1;

        // Create L2 provider to compute output roots
        let fetcher = op_succinct_host_utils::fetcher::OPSuccinctDataFetcher::new();
        let anchor_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(anchor_block)).await?;
        let anchor_extra_data = (U256::from(anchor_block), u32::MAX).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, anchor_root, anchor_extra_data.into())
            .value(init_bond)
            .send()
            .await?;
        let _receipt = tx.get_receipt().await?;

        let anchor_game_count = factory.gameCount().call().await?;
        let anchor_game_index = anchor_game_count - U256::from(1);
        let anchor_game_info = factory.gameAtIndex(anchor_game_index).call().await?;
        info!(
            "✓ Created valid anchor game at index {} (address: {})",
            anchor_game_index, anchor_game_info.proxy_
        );

        // Step 2: Create an invalid middle game with wrong output root
        let middle_block = anchor_block + 10;
        let mut rng = rand::rng();
        let mut invalid_root_bytes = [0u8; 32];
        rng.fill(&mut invalid_root_bytes);
        let invalid_root = FixedBytes::<32>::from(invalid_root_bytes);
        let middle_extra_data =
            (U256::from(middle_block), anchor_game_index.to::<u32>()).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, invalid_root, middle_extra_data.into())
            .value(init_bond)
            .send()
            .await?;
        tx.get_receipt().await?;

        let middle_game_count = factory.gameCount().call().await?;
        let middle_game_index = middle_game_count - U256::from(1);
        let middle_game_info = factory.gameAtIndex(middle_game_index).call().await?;
        info!(
            "✓ Created invalid middle game at index {} (address: {})",
            middle_game_index, middle_game_info.proxy_
        );

        // Step 3: Create a valid child game pointing to invalid parent
        let child_block = middle_block + 10;
        let child_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(child_block)).await?;
        let child_extra_data =
            (U256::from(child_block), middle_game_index.to::<u32>()).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, child_root, child_extra_data.into())
            .value(init_bond)
            .send()
            .await?;
        tx.get_receipt().await?;

        let child_game_count = factory.gameCount().call().await?;
        let child_game_index = child_game_count - U256::from(1);
        let child_game_info = factory.gameAtIndex(child_game_index).call().await?;
        info!(
            "✓ Created valid child game at index {} (address: {})",
            child_game_index, child_game_info.proxy_
        );

        // === PHASE 2: Start Proposer and Verify Chain Rejection ===
        info!("=== Phase 2: Starting Proposer to Validate Chain ===");

        // Start proposer
        let proposer_handle = start_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;
        info!("✓ Proposer service started");

        // Wait for proposer to create a new game (it should skip the invalid chain)
        let initial_game_count = child_game_count;
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
                let new_game = op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                new_game_info.proxy_,
                env.anvil.provider.clone(),
            );
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

        // Stop proposer
        proposer_handle.abort();
        info!("✓ Test complete: Proposer correctly rejected invalid parent chain");

        Ok(())
    }

    // Challenges the parent after a child is created and checks proposer behavior.
    // Confirms the chain with a challenged ancestor is abandoned for a fresh anchor.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_chain_validation_challenged_parent() -> Result<()> {
        TestEnvironment::init_logging();
        info!("=== Test: Game Chain Validation - Challenged Parent ===");

        // Setup common test environment
        let env = TestEnvironment::setup().await?;

        // Create a signer for creating games
        let wallet = PrivateKeySigner::from_str(PROPOSER_PRIVATE_KEY)?;
        let provider_with_signer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet))
            .connect_http(env.anvil.endpoint.parse::<Url>()?);

        let factory = DisputeGameFactory::new(env.deployed.factory, provider_with_signer.clone());
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        // CRITICAL FIX: Advance time after contract deployment
        // This ensures games created won't be considered "retired" (createdAt >
        // respectedGameTypeUpdatedAt)
        info!("Advancing time to ensure games won't be retired...");
        warp_time(&env.anvil.provider, Duration::from_secs(10)).await?;

        // === PHASE 1: Create Valid Parent Game ===
        info!("=== Phase 1: Creating Valid Parent Game ===");

        // Create a valid parent game that will be challenged
        let parent_block = env.anvil.starting_l2_block_number + 1;
        let fetcher = op_succinct_host_utils::fetcher::OPSuccinctDataFetcher::new();
        let parent_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(parent_block)).await?;
        let parent_extra_data = (U256::from(parent_block), u32::MAX).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, parent_root, parent_extra_data.into())
            .value(init_bond)
            .send()
            .await?;

        // Wait for transaction receipt with confirmations
        let receipt = tx.with_required_confirmations(3).get_receipt().await?;
        info!("Parent game creation tx confirmed in block {:?}", receipt.block_number);

        let parent_game_count = factory.gameCount().call().await?;
        let parent_game_index = parent_game_count - U256::from(1);
        let parent_game_info = factory.gameAtIndex(parent_game_index).call().await?;
        let parent_game_address = parent_game_info.proxy_;
        info!(
            "✓ Created valid parent game at index {} (address: {})",
            parent_game_index, parent_game_address
        );

        // === PHASE 2: Create Child Game Referencing Valid Parent ===
        info!("=== Phase 2: Creating Child Game with Valid Parent ===");

        // Double-check parent game status right before creating child
        let parent_game =
            op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                parent_game_address,
                provider_with_signer.clone(),
            );
        let parent_status = parent_game.status().call().await?;
        info!("Parent game status right before child creation: {:?}", parent_status);

        let was_respected = parent_game.wasRespectedGameTypeWhenCreated().call().await?;
        info!("Parent game wasRespectedGameTypeWhenCreated: {}", was_respected);

        // Check if parent game is blacklisted or retired via AnchorStateRegistry
        // We need to get the anchor state registry address from the parent game
        let anchor_registry_addr = parent_game.anchorStateRegistry().call().await?;
        info!("AnchorStateRegistry address: {:?}", anchor_registry_addr);

        // Check if the parent game passes the validation checks
        let anchor_registry = op_succinct_bindings::anchor_state_registry::AnchorStateRegistry::new(
            anchor_registry_addr,
            provider_with_signer.clone(),
        );

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

            // Get the portal address to check respectedGameTypeUpdatedAt
            let portal_addr = anchor_registry.portal().call().await?;
            let portal = op_succinct_bindings::mock_optimism_portal2::MockOptimismPortal2::new(
                portal_addr,
                provider_with_signer.clone(),
            );
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
        let child_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(child_block)).await?;
        let child_extra_data =
            (U256::from(child_block), parent_game_index.to::<u32>()).abi_encode_packed();

        info!(
            "Attempting to create child game with parent index: {}",
            parent_game_index.to::<u32>()
        );
        let tx = factory
            .create(TEST_GAME_TYPE, child_root, child_extra_data.into())
            .value(init_bond)
            .send()
            .await?;

        // Wait for transaction receipt with confirmations
        let receipt = tx.with_required_confirmations(3).get_receipt().await?;
        info!("Child game creation tx confirmed in block {:?}", receipt.block_number);

        let child_game_count = factory.gameCount().call().await?;
        let child_game_index = child_game_count - U256::from(1);
        let child_game_info = factory.gameAtIndex(child_game_index).call().await?;
        info!(
            "✓ Created child game at index {} (address: {})",
            child_game_index, child_game_info.proxy_
        );

        // === PHASE 3: Challenge and Resolve Parent as CHALLENGER_WINS ===
        info!("=== Phase 3: Challenging Parent Game (after child was created) ===");

        // Start challenger to challenge the game (using malicious mode to challenge valid game)
        let challenger_handle = start_challenger(
            &env.rpc_config,
            CHALLENGER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
            Some(100.0), // Challenge all games maliciously for testing
        )
        .await?;
        info!("✓ Challenger service started in malicious mode");

        // Wait for challenge
        wait_for_challenges(&env.anvil.provider, &[parent_game_address], Duration::from_secs(30))
            .await?;
        info!("✓ Parent game challenged");

        // Warp time to resolve as CHALLENGER_WINS (no proof submitted)
        warp_time(
            &env.anvil.provider,
            Duration::from_secs(MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION),
        )
        .await?;

        // Wait for resolution
        wait_and_verify_game_resolutions(
            &env.anvil.provider,
            &[parent_game_address],
            GameStatus::CHALLENGER_WINS,
            "ChallengerWins",
            Duration::from_secs(30),
        )
        .await?;
        info!("✓ Parent game resolved as CHALLENGER_WINS");

        // Stop challenger
        challenger_handle.abort();

        // === PHASE 4: Start Proposer and Verify Chain Rejection ===
        info!("=== Phase 4: Starting Proposer to Validate Chain ===");

        // Start proposer
        let proposer_handle = start_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;
        info!("✓ Proposer service started");

        // Wait for proposer to create a new game (it should skip the chain with challenged parent)
        let initial_game_count = child_game_count;
        let mut new_game_created = false;

        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let current_game_count = factory.gameCount().call().await?;

            if current_game_count > initial_game_count {
                new_game_created = true;
                let new_game_index = current_game_count - U256::from(1);
                let new_game_info = factory.gameAtIndex(new_game_index).call().await?;

                // Check that the new game doesn't build on the challenged parent chain
                let new_game = op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                new_game_info.proxy_,
                env.anvil.provider.clone(),
            );
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

        // Stop proposer
        proposer_handle.abort();
        info!("✓ Test complete: Proposer correctly rejected chain with challenged parent");

        Ok(())
    }

    // Resets the anchor to a new finalized chain and monitors proposer syncing.
    // Verifies new games follow the updated anchor branch after reset events.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_game_chain_validation_anchor_reset() -> Result<()> {
        TestEnvironment::init_logging();
        info!("=== Test: Game Chain Validation - Anchor Reset ===");

        // Setup common test environment
        let env = TestEnvironment::setup().await?;

        // Create a signer for creating games
        let wallet = PrivateKeySigner::from_str(PROPOSER_PRIVATE_KEY)?;
        let provider_with_signer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet))
            .connect_http(env.anvil.endpoint.parse::<Url>()?);

        let factory = DisputeGameFactory::new(env.deployed.factory, provider_with_signer.clone());
        let init_bond = factory.initBonds(TEST_GAME_TYPE).call().await?;

        // Prevent games from being retired immediately after deployment
        info!("Advancing time to ensure games won't be retired...");
        warp_time(&env.anvil.provider, Duration::from_secs(10)).await?;

        // === PHASE 1: Create initial canonical chain (A0 -> A1) ===
        info!("=== Phase 1: Creating Initial Canonical Chain ===");
        let fetcher = op_succinct_host_utils::fetcher::OPSuccinctDataFetcher::new();

        let a0_block = env.anvil.starting_l2_block_number + 1;
        let a0_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(a0_block)).await?;
        let a0_extra = (U256::from(a0_block), u32::MAX).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, a0_root, a0_extra.into())
            .value(init_bond)
            .send()
            .await?;
        let receipt = tx.with_required_confirmations(3).get_receipt().await?;
        info!("Game A0 creation tx confirmed in block {:?}", receipt.block_number);

        let game_count = factory.gameCount().call().await?;
        let a0_index = game_count - U256::from(1);
        let a0_info = factory.gameAtIndex(a0_index).call().await?;
        let a0_address = a0_info.proxy_;
        info!("✓ Created game A0 at index {} (address: {})", a0_index, a0_address);

        let a1_block = a0_block + 10;
        let a1_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(a1_block)).await?;
        let a1_extra = (U256::from(a1_block), a0_index.to::<u32>()).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, a1_root, a1_extra.into())
            .value(init_bond)
            .send()
            .await?;
        let receipt = tx.with_required_confirmations(3).get_receipt().await?;
        info!("Game A1 creation tx confirmed in block {:?}", receipt.block_number);

        let game_count = factory.gameCount().call().await?;
        let a1_index = game_count - U256::from(1);
        let a1_info = factory.gameAtIndex(a1_index).call().await?;
        let a1_address = a1_info.proxy_;
        info!("✓ Created game A1 at index {} (address: {})", a1_index, a1_address);

        // Record current anchor game
        let anchor_registry_addr =
            op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                a0_address,
                provider_with_signer.clone(),
            )
            .anchorStateRegistry()
            .call()
            .await?;
        let anchor_registry = op_succinct_bindings::anchor_state_registry::AnchorStateRegistry::new(
            anchor_registry_addr,
            provider_with_signer.clone(),
        );
        let initial_anchor = anchor_registry.anchorGame().call().await?;
        info!("Initial anchor game: {}", initial_anchor);

        // === PHASE 2: Create new anchor chain (B0 -> B1) ===
        info!("=== Phase 2: Creating Alternate Anchor Chain ===");
        let b0_block = a1_block + 10;
        let b0_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(b0_block)).await?;
        let b0_extra = (U256::from(b0_block), u32::MAX).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, b0_root, b0_extra.into())
            .value(init_bond)
            .send()
            .await?;
        let receipt = tx.with_required_confirmations(3).get_receipt().await?;
        info!("Game B0 creation tx confirmed in block {:?}", receipt.block_number);

        let game_count = factory.gameCount().call().await?;
        let b0_index = game_count - U256::from(1);
        let b0_info = factory.gameAtIndex(b0_index).call().await?;
        let b0_address = b0_info.proxy_;
        info!("✓ Created game B0 at index {} (address: {})", b0_index, b0_address);

        let b1_block = b0_block + 10;
        let b1_root =
            fetcher.l2_provider.compute_output_root_at_block(U256::from(b1_block)).await?;
        let b1_extra = (U256::from(b1_block), b0_index.to::<u32>()).abi_encode_packed();

        let tx = factory
            .create(TEST_GAME_TYPE, b1_root, b1_extra.into())
            .value(init_bond)
            .send()
            .await?;
        let receipt = tx.with_required_confirmations(3).get_receipt().await?;
        info!("Game B1 creation tx confirmed in block {:?}", receipt.block_number);

        let game_count = factory.gameCount().call().await?;
        let b1_index = game_count - U256::from(1);
        let b1_info = factory.gameAtIndex(b1_index).call().await?;
        let b1_address = b1_info.proxy_;
        info!("✓ Created game B1 at index {} (address: {})", b1_index, b1_address);

        // Resolve the new chain so it becomes eligible as the anchor
        let finality_wait =
            DISPUTE_GAME_FINALITY_DELAY_SECONDS + MAX_CHALLENGE_DURATION + MAX_PROVE_DURATION + 60;
        info!("Warping time by {} seconds to finalize new chain", finality_wait);
        warp_time(&env.anvil.provider, Duration::from_secs(finality_wait)).await?;

        let b0_game =
            op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                b0_address,
                provider_with_signer.clone(),
            );
        let tx = b0_game.resolve().send().await?;
        let receipt = tx.with_required_confirmations(1).get_receipt().await?;
        info!("Game B0 resolved in tx {:?}", receipt.transaction_hash);

        let b1_game =
            op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                b1_address,
                provider_with_signer.clone(),
            );
        let tx = b1_game.resolve().send().await?;
        let receipt = tx.with_required_confirmations(1).get_receipt().await?;
        info!("Game B1 resolved in tx {:?}", receipt.transaction_hash);

        // Advance beyond finality delay so the new chain is finalized
        info!(
            "Warping time by {} seconds to finalize resolved games",
            DISPUTE_GAME_FINALITY_DELAY_SECONDS + 60
        );
        warp_time(
            &env.anvil.provider,
            Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 60),
        )
        .await?;

        // Force anchor to update to the new chain rooted at B0
        let tx = anchor_registry
            .setAnchorState(b0_address)
            .send()
            .await?
            .with_required_confirmations(1)
            .get_receipt()
            .await?;
        info!("Anchor reset tx confirmed in block {:?}", tx.block_number);

        // Ensure anchor switched to new chain
        let updated_anchor = anchor_registry.anchorGame().call().await?;
        info!("Updated anchor game: {}", updated_anchor);
        assert_ne!(
            updated_anchor, initial_anchor,
            "Anchor should update to the new branch before starting proposer"
        );

        // === PHASE 3: Start proposer and verify it follows new anchor chain ===
        info!("=== Phase 3: Starting Proposer to Observe Anchor Reset ===");
        let proposer_handle = start_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;
        info!("✓ Proposer service started");

        let initial_game_count = game_count;
        let mut new_game_parent_verified = false;

        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let current_count = factory.gameCount().call().await?;

            if current_count > initial_game_count {
                let new_game_index = current_count - U256::from(1);
                let new_game_info = factory.gameAtIndex(new_game_index).call().await?;
                let new_game = op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                new_game_info.proxy_,
                env.anvil.provider.clone(),
            );
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
        TestEnvironment::init_logging();
        info!("=== Test: Proposer Recovery After Runtime Canonical Head Invalidation ===");

        // Setup common test environment
        let env = TestEnvironment::setup().await?;

        // === PHASE 1: Start Proposer and Create Initial Games ===
        info!("=== Phase 1: Start Proposer and Create Games ===");

        let proposer_handle = start_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;
        info!("✓ Proposer service started");

        // Wait for proposer to create 3 games
        let factory = DisputeGameFactory::new(env.deployed.factory, env.anvil.provider.clone());
        let tracked_games =
            wait_and_track_games(&factory, TEST_GAME_TYPE, 3, Duration::from_secs(150)).await?;
        info!("✓ Proposer created {} games:", tracked_games.len());

        assert!(!proposer_handle.is_finished(), "Proposer should be running");

        // === PHASE 2: Challenge Game 3 =================================
        info!("=== Phase 2: Challenge Game 3 ===");
        let challenger = init_challenger(
            &env.rpc_config,
            CHALLENGER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
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
        warp_time(&env.anvil.provider, Duration::from_secs(MAX_CHALLENGE_DURATION)).await?;
        info!("✓ Warped time by MAX_CHALLENGE_DURATION to enable resolution for first 2 games");

        // Resolve first 2 games as DEFENDER_WINS
        let proposer = init_proposer(
            &env.rpc_config,
            PROPOSER_PRIVATE_KEY,
            &env.deployed.factory,
            TEST_GAME_TYPE,
        )
        .await?;

        let first_two_games = &tracked_games[0..2];
        for game in first_two_games {
            let game = ProposerGame {
                index: game.index,
                address: game.address,
                parent_index: game.parent_index,
                l2_block: game.l2_block_number,
                status: GameStatus::IN_PROGRESS,
                proposal_status: ProposalStatus::Unchallenged,
                deadline: 0,
                should_attempt_to_resolve: true,
                should_attempt_to_claim_bond: false,
            };
            proposer.submit_resolution_transaction(&game).await?;
        }
        let resolutions =
            wait_for_resolutions(&env.anvil.provider, first_two_games, Duration::from_secs(60))
                .await?;
        verify_all_resolved_correctly(&resolutions)?;
        info!("✓ First 2 games resolved as DEFENDER_WINS");

        // Warp time to allow challenged game 3 to be resolved as CHALLENGER_WINS
        warp_time(&env.anvil.provider, Duration::from_secs(MAX_PROVE_DURATION)).await?;
        challenger.submit_resolution_transaction(&game_to_challenge).await?;
        info!("✓ Challenger resolved game 3 as CHALLENGER_WINS");

        assert!(!proposer_handle.is_finished(), "Proposer should still be running");

        // Warp time to allow the proposer to finalize the first 2 games
        warp_time(&env.anvil.provider, Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS))
            .await?;
        for game in first_two_games {
            let game = ProposerGame {
                index: game.index,
                address: game.address,
                parent_index: game.parent_index,
                l2_block: game.l2_block_number,
                status: GameStatus::DEFENDER_WINS,
                proposal_status: ProposalStatus::Resolved,
                deadline: 0,
                should_attempt_to_resolve: false,
                should_attempt_to_claim_bond: true,
            };
            proposer.submit_bond_claim_transaction(&game).await?;
        }
        info!("✓ Proposer finalized the first 2 games");

        // === PHASE 4: Verify Proposer recovers automatically ===
        info!("=== Phase 4: Verify Proposer recovers automatically ===");
        info!("Proposer should create a new game from the last valid game at index 1");

        let mut current_game_count = factory.gameCount().call().await?;
        info!("Current game count: {}", current_game_count);

        while current_game_count <= U256::from(3) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            current_game_count = factory.gameCount().call().await?;
        }

        // Verify the new game is built on the last valid game at index 1
        let new_game_index = U256::from(3);
        let new_game_info = factory.gameAtIndex(new_game_index).call().await?;

        let new_game =
            op_succinct_bindings::op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::new(
                new_game_info.proxy_,
                env.anvil.provider.clone(),
            );
        let claim_data = new_game.claimData().call().await?;

        assert_eq!(
            claim_data.parentIndex, 1u32,
            "Proposer should create a new game from the last valid game"
        );

        // Verify proposer continues to operate normally
        assert!(!proposer_handle.is_finished(), "Proposer should continue running after recovery");

        // Stop proposer
        proposer_handle.abort();
        info!("✓ Proposer stopped");

        Ok(())
    }
}
