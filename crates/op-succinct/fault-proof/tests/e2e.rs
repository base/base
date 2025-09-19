mod common;

use std::str::FromStr;

use alloy_network::EthereumWallet;
use alloy_primitives::{Bytes, FixedBytes, U256};
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use fault_proof::contract::GameStatus;
use op_succinct_bindings::{
    dispute_game_factory::DisputeGameFactory, mock_optimism_portal2::MockOptimismPortal2,
};
use op_succinct_signer_utils::Signer;
use rand::Rng;
use tokio::time::{sleep, Duration};
use tracing::info;

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

use crate::common::{start_challenger, start_proposer};

alloy_sol_types::sol! {
    #[sol(rpc)]
    contract MockPermissionedDisputeGameView {
        function status() external view returns (uint8);
    }
}

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
        wait_and_track_games(&factory, TEST_GAME_TYPE, 3, Duration::from_secs(60)).await?;

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
        wait_for_resolutions(&env.anvil.provider, &tracked_games, Duration::from_secs(30)).await?;

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
        Duration::from_secs(60),
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

#[tokio::test(flavor = "multi_thread")]
async fn test_game_type_transition_skips_legacy_games() -> Result<()> {
    TestEnvironment::init_logging();
    info!("=== Test: Game Type Transition With Legacy Games In History ===");

    let env = TestEnvironment::setup().await?;
    let mut rng = rand::rng();

    let l1_rpc_url = env.rpc_config.l1_rpc.clone();
    let factory_reader = DisputeGameFactory::new(env.deployed.factory, env.anvil.provider.clone());
    let initial_game_count = factory_reader.gameCount().call().await?;
    let init_bond = factory_reader.initBonds(TEST_GAME_TYPE).call().await?;

    let proposer_signer = Signer::new_local_signer(PROPOSER_PRIVATE_KEY)?;

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

    let legacy_game_type_call =
        MockOptimismPortal2::setRespectedGameTypeCall { _gameType: MOCK_PERMISSIONED_GAME_TYPE };
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

        let game_info = factory_reader.gameAtIndex(expected_index - U256::from(1)).call().await?;
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
        wait_for_resolutions(&env.anvil.provider, &tracked_games, Duration::from_secs(60)).await?;
    verify_all_resolved_correctly(&resolutions)?;

    warp_time(&env.anvil.provider, Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS))
        .await?;
    wait_for_bond_claims(
        &env.anvil.provider,
        &tracked_games,
        PROPOSER_ADDRESS,
        Duration::from_secs(90),
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
    warp_time(&env.anvil.provider, Duration::from_secs(DISPUTE_GAME_FINALITY_DELAY_SECONDS + 1))
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
            address,
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
