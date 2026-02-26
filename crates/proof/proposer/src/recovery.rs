//! Parent game state recovery from onchain data.

use alloy_primitives::B256;
use eyre::Result;
use tracing::info;

use crate::contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient,
};

/// Recovers parent game state from onchain data on startup.
///
/// Walks backwards through the `DisputeGameFactory` to find the most recent
/// game of the correct `game_type`. Returns `(game_index, output_root, l2_block_number)`
/// if found, or `None` if no matching game exists.
pub async fn recover_parent_game_state_standalone(
    factory: &DisputeGameFactoryContractClient,
    verifier: &AggregateVerifierContractClient,
    game_type: u32,
) -> Result<Option<(u32, B256, u64)>> {
    let count = factory.game_count().await?;
    if count == 0 {
        info!("No existing games found, will start from anchor registry");
        return Ok(None);
    }

    let search_count = count.min(crate::MAX_GAME_RECOVERY_LOOKBACK);

    for i in 0..search_count {
        let game_index = count - 1 - i;
        let game = factory.game_at_index(game_index).await?;

        if game.game_type != game_type {
            continue;
        }

        let game_info = verifier.game_info(game.proxy).await?;

        let idx: u32 = game_index
            .try_into()
            .map_err(|_| eyre::eyre!("game index {game_index} exceeds u32"))?;

        info!(
            game_index,
            game_proxy = %game.proxy,
            output_root = ?game_info.root_claim,
            l2_block_number = game_info.l2_block_number,
            "Recovered parent game state from onchain"
        );
        return Ok(Some((idx, game_info.root_claim, game_info.l2_block_number)));
    }

    info!(
        game_type,
        searched = search_count,
        "No games found for our game type in recent history, will start from anchor registry"
    );
    Ok(None)
}
