//! Event monitoring and state tracking utilities for E2E tests.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use anyhow::Result;
use fault_proof::contract::{GameStatus, ProposalStatus};
use op_succinct_bindings::{
    dispute_game_factory::DisputeGameFactory,
    op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame,
};
use tokio::time::{sleep, Instant};
use tracing::info;

/// Represents a tracked game for monitoring
#[derive(Debug, Clone)]
pub struct TrackedGame {
    pub address: Address,
    pub l2_block_number: U256,
}

/// Wait for N games to be created and return their info
pub async fn wait_and_track_games<P: Provider>(
    factory: &DisputeGameFactory::DisputeGameFactoryInstance<P>,
    game_type: u32,
    count: usize,
    timeout_duration: Duration,
) -> Result<Vec<TrackedGame>> {
    info!("Waiting for {} games to be created...", count);

    let start_time = Instant::now();
    let mut tracked_games = Vec::new();
    let mut last_game_count = U256::ZERO;

    // Get initial game count
    let initial_count = factory.gameCount().call().await?;
    info!("Initial game count: {}", initial_count);

    loop {
        // Check timeout
        if start_time.elapsed() > timeout_duration {
            anyhow::bail!(
                "Timeout waiting for games. Got {} out of {} games",
                tracked_games.len(),
                count
            );
        }

        // Get current game count
        let current_count = factory.gameCount().call().await?;

        // Check for new games
        if current_count > last_game_count {
            for i in last_game_count.to::<u64>()..current_count.to::<u64>() {
                let game_info = factory.gameAtIndex(U256::from(i)).call().await?;

                // Check if it's our game type
                if game_info.gameType_ == game_type {
                    let game =
                        OPSuccinctFaultDisputeGame::new(game_info.proxy_, factory.provider());

                    // Get game details
                    let l2_block_number = game.l2BlockNumber().call().await?;

                    let tracked = TrackedGame { address: game_info.proxy_, l2_block_number };

                    info!(
                        "Tracked game {}/{}: {} at L2 block {}",
                        tracked_games.len() + 1,
                        count,
                        tracked.address,
                        tracked.l2_block_number
                    );

                    tracked_games.push(tracked);

                    if tracked_games.len() >= count {
                        return Ok(tracked_games);
                    }
                }
            }

            last_game_count = current_count;
        }

        // Wait before checking again
        sleep(Duration::from_secs(1)).await;
    }
}

/// Wait for games to be resolved
pub async fn wait_for_resolutions<P: Provider>(
    provider: &P,
    tracked_games: &[TrackedGame],
    timeout_duration: Duration,
) -> Result<Vec<GameStatus>> {
    info!("Waiting for {} games to be resolved...", tracked_games.len());

    let deadline = Instant::now() + timeout_duration;
    let mut statuses = vec![GameStatus::IN_PROGRESS; tracked_games.len()];

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("Timeout waiting for game resolutions");
        }

        for (i, game) in tracked_games.iter().enumerate() {
            let game_contract = OPSuccinctFaultDisputeGame::new(game.address, provider);
            let status = GameStatus::try_from(game_contract.status().call().await?)?;

            statuses[i] = status;

            if status != GameStatus::IN_PROGRESS {
                info!("Game {} resolved with status: {:?}", game.address, status);
            }
        }

        if statuses.iter().all(|&status| status != GameStatus::IN_PROGRESS) {
            return Ok(statuses);
        }

        sleep(Duration::from_secs(2)).await;
    }
}

/// Wait for challenges to be submitted to games
pub async fn wait_for_challenges<P: Provider>(
    provider: &P,
    game_addresses: &[Address],
    timeout_duration: Duration,
) -> Result<Vec<bool>> {
    info!("Waiting for challenges on {} games...", game_addresses.len());

    // Wait for 10 seconds to allow for challenges to be submitted
    sleep(Duration::from_secs(10)).await;

    let deadline = Instant::now() + timeout_duration;
    let mut statuses = vec![false; game_addresses.len()];

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("Timeout waiting for challenges");
        }

        for (i, &game_address) in game_addresses.iter().enumerate() {
            let game = OPSuccinctFaultDisputeGame::new(game_address, provider);
            let claim_data = game.claimData().call().await?;
            let claim_data_status = ProposalStatus::try_from(claim_data.status)?;

            statuses[i] = claim_data_status == ProposalStatus::Challenged;

            if claim_data.status != 0 {
                info!("Game {} status: {:?}", game_address, claim_data_status);
            }
        }

        if statuses.iter().all(|&challenged| challenged) {
            return Ok(statuses);
        }

        sleep(Duration::from_secs(2)).await;
    }
}

/// Wait for bond claims to be made by checking normalModeCredit
pub async fn wait_for_bond_claims<P: Provider>(
    provider: &P,
    tracked_games: &[TrackedGame],
    recipient_address: Address,
    timeout_duration: Duration,
) -> Result<()> {
    info!(
        "Waiting for bond claims on {} games for recipient {}...",
        tracked_games.len(),
        recipient_address
    );

    let start_time = Instant::now();
    let deadline = start_time + timeout_duration;
    let mut claims = vec![false; tracked_games.len()];

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("Timeout waiting for bond claims");
        }

        for (i, game) in tracked_games.iter().enumerate() {
            if claims[i] {
                continue; // Already claimed
            }

            let game_contract = OPSuccinctFaultDisputeGame::new(game.address, provider);

            // Check both normalModeCredit and refundModeCredit balances
            // When credit is claimed, both should be 0
            let normal_credit = game_contract.normalModeCredit(recipient_address).call().await?;
            let refund_credit = game_contract.refundModeCredit(recipient_address).call().await?;

            // If both credits are zero, the claim has been made
            if normal_credit == U256::ZERO && refund_credit == U256::ZERO {
                claims[i] = true;
                info!("Bonds claimed for game {}", game.address);
            } else {
                // Log current credit balances for debugging
                if i == 0 && start_time.elapsed().as_secs().is_multiple_of(10) {
                    info!(
                        "Game {} - Recipient {} normalModeCredit: {}, refundModeCredit: {}",
                        game.address, recipient_address, normal_credit, refund_credit
                    );
                }
            }
        }

        if claims.iter().all(|&claimed| claimed) {
            return Ok(());
        }

        sleep(Duration::from_secs(2)).await;
    }
}

/// Verify all games resolved with expected status
pub fn verify_games_resolved(
    statuses: &[GameStatus],
    expected_status: GameStatus,
    winner_name: &str,
) -> Result<()> {
    if let Some((i, &status)) =
        statuses.iter().enumerate().find(|(_, &status)| status != expected_status)
    {
        anyhow::bail!(
            "Game {} did not resolve to {}: got status {:?} instead",
            i,
            winner_name,
            status
        );
    }
    info!("All {} games resolved correctly ({})", statuses.len(), winner_name);
    Ok(())
}

/// Verify all games resolved correctly (proposer wins)
pub fn verify_all_resolved_correctly(statuses: &[GameStatus]) -> Result<()> {
    verify_games_resolved(statuses, GameStatus::DEFENDER_WINS, "ProposerWins")
}

/// Wait for games to resolve and verify they match expected status
pub async fn wait_and_verify_game_resolutions<P: Provider>(
    provider: &P,
    game_addresses: &[Address],
    expected_status: GameStatus,
    winner_name: &str,
    timeout_duration: Duration,
) -> Result<()> {
    info!("Waiting for {} games to resolve as {}...", game_addresses.len(), winner_name);

    // Wait for 10 seconds for the games to be resolved
    sleep(Duration::from_secs(10)).await;

    let deadline = Instant::now() + timeout_duration;

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("Timeout waiting for game resolutions");
        }

        let mut statuses = Vec::new();

        for &game_address in game_addresses.iter() {
            let game = OPSuccinctFaultDisputeGame::new(game_address, provider);
            let status = GameStatus::try_from(game.status().call().await?)?;
            statuses.push(status);
        }

        if statuses.iter().all(|&status| status != GameStatus::IN_PROGRESS) {
            // Verify all games resolved with expected status
            verify_games_resolved(&statuses, expected_status, winner_name)?;
            return Ok(());
        }

        sleep(Duration::from_secs(2)).await;
    }
}
