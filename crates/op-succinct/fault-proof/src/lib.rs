pub mod backup;
pub mod challenger;
pub mod config;
pub mod contract;
pub mod prometheus;
pub mod proposer;
pub mod prover;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{address, keccak256, Address, FixedBytes, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::Block;
use alloy_sol_types::SolValue;
use anyhow::{bail, Result};
use async_trait::async_trait;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;

use crate::contract::{
    DisputeGameFactory::DisputeGameFactoryInstance, GameStatus, IDisputeGame, L2Output,
    OPSuccinctFaultDisputeGame,
};

pub type L1Provider = RootProvider;
pub type L2Provider = RootProvider<Optimism>;
pub type L2NodeProvider = RootProvider<Optimism>;

pub const NUM_CONFIRMATIONS: u64 = 3;
pub const TIMEOUT_SECONDS: u64 = 60;

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
        let block = self.get_block_by_number(block_number).await?;
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
        let storage_root =
            self.get_proof(address, Vec::new()).block_id(block_number.into()).await?.storage_hash;
        Ok(storage_root)
    }

    /// Compute the output root at a given L2 block number.
    ///
    /// Local implementation is used because the RPC method `optimism_outputAtBlock` can fail for
    /// older blocks if the L2 node isn't fully synced or has pruned historical state data.
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
pub trait FactoryTrait<P>
where
    P: Provider + Clone,
{
    /// Returns the game implementation for the given game type.
    /// Errors if the game type is not registered (zero address).
    async fn game_impl(
        &self,
        game_type: u32,
    ) -> Result<OPSuccinctFaultDisputeGame::OPSuccinctFaultDisputeGameInstance<P>>;

    /// Fetches the bond required to create a game.
    async fn fetch_init_bond(&self, game_type: u32) -> Result<U256>;

    /// Fetches the latest game index.
    async fn fetch_latest_game_index(&self) -> Result<Option<U256>>;
}

#[async_trait]
impl<P> FactoryTrait<P> for DisputeGameFactoryInstance<P>
where
    P: Provider + Clone,
{
    /// Returns the game implementation for the given game type.
    /// Errors if the game type is not registered (zero address).
    async fn game_impl(
        &self,
        game_type: u32,
    ) -> Result<OPSuccinctFaultDisputeGame::OPSuccinctFaultDisputeGameInstance<P>> {
        let game_impl_address = self.gameImpls(game_type).call().await?;
        if game_impl_address == Address::ZERO {
            bail!("Game type {game_type} is not registered in the factory");
        }
        Ok(OPSuccinctFaultDisputeGame::new(game_impl_address, self.provider().clone()))
    }

    /// Fetches the bond required to create a game.
    async fn fetch_init_bond(&self, game_type: u32) -> Result<U256> {
        let init_bond = self.initBonds(game_type).call().await?;
        Ok(init_bond)
    }

    /// Fetches the latest game index.
    async fn fetch_latest_game_index(&self) -> Result<Option<U256>> {
        let game_count = self.gameCount().call().await?;

        if game_count == U256::ZERO {
            tracing::debug!("No games exist yet");
            return Ok(None);
        }

        let latest_game_index = game_count - U256::from(1);
        tracing::debug!("Latest game index: {:?}", latest_game_index);

        Ok(Some(latest_game_index))
    }
}

async fn is_parent_resolved<P>(
    parent_index: u32,
    factory: &DisputeGameFactoryInstance<P>,
) -> Result<bool>
where
    P: Provider + Clone,
{
    if parent_index == u32::MAX {
        return Ok(true);
    }

    let parent_game_address = factory.gameAtIndex(U256::from(parent_index)).call().await?.proxy;
    let parent_game_contract = IDisputeGame::new(parent_game_address, factory.provider());

    Ok(parent_game_contract.status().call().await? != GameStatus::IN_PROGRESS)
}

async fn is_parent_challenger_wins<P>(
    parent_index: u32,
    factory: &DisputeGameFactoryInstance<P>,
) -> Result<bool>
where
    P: Provider + Clone,
{
    if parent_index == u32::MAX {
        return Ok(false);
    }

    let parent_game_address = factory.gameAtIndex(U256::from(parent_index)).call().await?.proxy;
    let parent_game_contract = IDisputeGame::new(parent_game_address, factory.provider());

    Ok(parent_game_contract.status().call().await? == GameStatus::CHALLENGER_WINS)
}

/// Prefix used for transaction revert errors.
pub const TX_REVERTED_PREFIX: &str = "transaction reverted:";

/// Extension trait for checking transaction error types.
pub trait TxErrorExt {
    /// Returns true if this error indicates a transaction revert (definitive failure).
    fn is_revert(&self) -> bool;
}

impl TxErrorExt for anyhow::Error {
    fn is_revert(&self) -> bool {
        self.to_string().starts_with(TX_REVERTED_PREFIX)
    }
}
