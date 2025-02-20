pub mod config;
pub mod contract;
pub mod utils;

use alloy_eips::BlockNumberOrTag;
use alloy_network::Ethereum;
use alloy_primitives::{address, keccak256, Address, FixedBytes, B256, U256};
use alloy_provider::{
    fillers::{FillProvider, TxFiller},
    Provider, RootProvider,
};
use alloy_rpc_types::eth::Block;
use alloy_sol_types::SolValue;
use anyhow::{bail, Result};
use async_trait::async_trait;
use op_alloy_network::{primitives::BlockTransactionsKind, Optimism};
use op_alloy_rpc_types::Transaction;

use crate::contract::{
    AnchorStateRegistry, DisputeGameFactory::DisputeGameFactoryInstance, L2Output,
    OPSuccinctFaultDisputeGame,
};

pub type L1Provider = RootProvider;
pub type L2Provider = RootProvider<Optimism>;
pub type L2NodeProvider = RootProvider<Optimism>;
pub type L1ProviderWithWallet<F, P> = FillProvider<F, P, Ethereum>;

#[async_trait]
pub trait L2ProviderTrait {
    /// Get the L2 block by number.
    async fn get_l2_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Block<Transaction>>;

    /// Get the L2 storage root for an address at a given block number.
    async fn get_l2_storage_root(
        &self,
        address: Address,
        block_number: BlockNumberOrTag,
    ) -> Result<B256>;

    /// Compute the output root at a given L2 block number.
    async fn compute_output_root_at_block(&self, l2_block_number: U256) -> Result<FixedBytes<32>>;
}

#[async_trait]
impl L2ProviderTrait for L2Provider {
    /// Get the L2 block by number.
    async fn get_l2_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Block<Transaction>> {
        let block = self
            .get_block_by_number(block_number, BlockTransactionsKind::Hashes)
            .await?;
        if let Some(block) = block {
            Ok(block)
        } else {
            bail!("Failed to get L2 block by number");
        }
    }

    /// Get the L2 storage root for an address at a given block number.
    async fn get_l2_storage_root(
        &self,
        address: Address,
        block_number: BlockNumberOrTag,
    ) -> Result<B256> {
        let storage_root = self
            .get_proof(address, Vec::new())
            .block_id(block_number.into())
            .await?
            .storage_hash;
        Ok(storage_root)
    }

    /// Compute the output root at a given L2 block number.
    ///
    /// Local implementation is used because the RPC method `optimism_outputAtBlock` can fail for older
    /// blocks if the L2 node isn't fully synced or has pruned historical state data.
    ///
    /// Common error: "missing trie node ... state is not available".
    async fn compute_output_root_at_block(&self, l2_block_number: U256) -> Result<FixedBytes<32>> {
        let l2_block = self
            .get_l2_block_by_number(BlockNumberOrTag::Number(l2_block_number.to::<u64>()))
            .await?;
        let l2_state_root = l2_block.header.state_root;
        let l2_claim_hash = l2_block.header.hash;
        let l2_storage_root = self
            .get_l2_storage_root(
                address!("0x4200000000000000000000000000000000000016"),
                BlockNumberOrTag::Number(l2_block_number.to::<u64>()),
            )
            .await?;

        let l2_claim_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_state_root.0.into(),
            l2_storage_hash: l2_storage_root.0.into(),
            l2_claim_hash: l2_claim_hash.0.into(),
        };
        let l2_output_root = keccak256(l2_claim_encoded.abi_encode());
        Ok(l2_output_root)
    }
}

#[async_trait]
pub trait FactoryTrait<F, P> {
    /// Fetches the bond required to create a game.
    async fn fetch_init_bond(&self, game_type: u32) -> Result<U256>;

    /// Fetches the latest game index.
    async fn fetch_latest_game_index(&self) -> Result<Option<U256>>;

    /// Fetches the game address by index.
    async fn fetch_game_address_by_index(&self, game_index: U256) -> Result<Address>;

    /// Get the latest valid proposal.
    ///
    /// This function checks from the latest game to the earliest game, returning the latest valid proposal.
    async fn get_latest_valid_proposal(
        &self,
        l2_provider: L2Provider,
    ) -> Result<Option<(U256, U256)>>;

    /// Get the anchor L2 block number.
    ///
    /// This function returns the L2 block number of the anchor game for a given game type.
    async fn get_anchor_l2_block_number(&self, game_type: u32) -> Result<U256>;
}

#[async_trait]
impl<F, P> FactoryTrait<F, P> for DisputeGameFactoryInstance<(), L1ProviderWithWallet<F, P>>
where
    F: TxFiller,
    P: Provider + Clone,
{
    /// Fetches the bond required to create a game.
    async fn fetch_init_bond(&self, game_type: u32) -> Result<U256> {
        let init_bond = self.initBonds(game_type).call().await?;
        Ok(init_bond._0)
    }

    /// Fetches the latest game index.
    async fn fetch_latest_game_index(&self) -> Result<Option<U256>> {
        let game_count = self.gameCount().call().await?;

        if game_count.gameCount_ == U256::ZERO {
            tracing::info!("No games exist yet");
            return Ok(None);
        }

        let latest_game_index = game_count.gameCount_ - U256::from(1);
        tracing::info!("Latest game index: {:?}", latest_game_index);

        Ok(Some(latest_game_index))
    }

    /// Fetches the game address by index.
    async fn fetch_game_address_by_index(&self, game_index: U256) -> Result<Address> {
        let game = self.gameAtIndex(game_index).call().await?;
        Ok(game.proxy)
    }

    /// Get the latest valid proposal.
    ///
    /// This function checks from the latest game to the earliest game, returning the latest valid proposal.
    async fn get_latest_valid_proposal(
        &self,
        l2_provider: L2Provider,
    ) -> Result<Option<(U256, U256)>> {
        // Get latest game index, return None if no games exist.
        let Some(mut game_index) = self.fetch_latest_game_index().await? else {
            tracing::info!("No games exist yet");
            return Ok(None);
        };

        let mut block_number;

        // Loop through games in reverse order (latest to earliest) to find the most recent valid game
        loop {
            // Get the game contract for the current index.
            let game_address = self.fetch_game_address_by_index(game_index).await?;
            let game = OPSuccinctFaultDisputeGame::new(game_address, self.provider());

            // Get the L2 block number the game is proposing output for.
            block_number = game.l2BlockNumber().call().await?.l2BlockNumber_;
            tracing::debug!(
                "Checking if game {:?} at block {:?} is valid",
                game_address,
                block_number
            );

            // Get the output root the game is proposing.
            let game_claim = game.rootClaim().call().await?.rootClaim_;

            // Compute the actual output root at the L2 block number.
            let output_root = l2_provider
                .compute_output_root_at_block(block_number)
                .await?;

            // If the output root matches the game claim, we've found the latest valid proposal.
            if output_root == game_claim {
                break;
            }

            // If the output root doesn't match the game claim, we need to find earlier games.
            tracing::info!(
                "Output root {:?} is not same as game claim {:?}",
                output_root,
                game_claim
            );

            // If we've reached index 0 (the earliest game) and still haven't found a valid proposal.
            // Return `None` as no valid proposals were found.
            if game_index == U256::ZERO {
                tracing::info!("No valid proposals found after checking all games");
                return Ok(None);
            }

            // Decrement the game index to check the previous game.
            game_index -= U256::from(1);
        }

        tracing::info!(
            "Latest valid proposal at game index {:?} with l2 block number: {:?}",
            game_index,
            block_number
        );

        Ok(Some((block_number, game_index)))
    }

    /// Get the anchor L2 block number.
    ///
    /// This function returns the L2 block number of the anchor game for a given game type.
    async fn get_anchor_l2_block_number(&self, game_type: u32) -> Result<U256> {
        let game_impl_address = self.gameImpls(game_type).call().await?._0;
        let game_impl = OPSuccinctFaultDisputeGame::new(game_impl_address, self.provider());
        let anchor_state_registry = AnchorStateRegistry::new(
            game_impl.anchorStateRegistry().call().await?.registry_,
            self.provider(),
        );
        let anchor_l2_block_number = anchor_state_registry.getAnchorRoot().call().await?._1;
        Ok(anchor_l2_block_number)
    }
}
