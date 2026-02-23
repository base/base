//! Traits for the Flashblocks module.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, TxHash, U256};
use alloy_rpc_types_eth::{Filter, Log, state::StateOverride};
use arc_swap::Guard;
use base_alloy_network::Base;
use base_primitives::Flashblock;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use tokio::sync::broadcast;

use crate::PendingBlocks;

/// Trait for receiving flashblock updates.
pub trait FlashblocksReceiver {
    /// Called when a new flashblock is received.
    fn on_flashblock_received(&self, flashblock: Flashblock);
}

/// Core API for accessing flashblock state and data.
pub trait FlashblocksAPI {
    /// Retrieves the pending blocks.
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>>;

    /// Subscribes to flashblock updates.
    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>>;
}

/// API for accessing pending blocks data.
pub trait PendingBlocksAPI {
    /// Get the canonical block number on top of which all pending state is built
    fn get_canonical_block_number(&self) -> BlockNumberOrTag;

    /// Get the pending transactions count for an address
    fn get_transaction_count(&self, address: Address) -> U256;

    /// Retrieves the current block. If `full` is true, includes full transaction details.
    fn get_block(&self, full: bool) -> Option<RpcBlock<Base>>;

    /// Gets transaction receipt by hash.
    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Base>>;

    /// Gets transaction details by hash.
    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Base>>;

    /// Gets balance for an address. Returns None if address not updated in flashblocks.
    fn get_balance(&self, address: Address) -> Option<U256>;

    /// Gets the state overrides for the pending blocks
    fn get_state_overrides(&self) -> Option<StateOverride>;

    /// Gets logs from pending state matching the provided filter.
    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log>;
}
