//! RPC response types and provider aliases for Base proof clients.

use alloy_network::Ethereum;
use alloy_provider::RootProvider;
use base_common_network::Base;
pub use base_optimism_rpc::{
    GenesisL2BlockRef, L1BlockId, L1BlockRef, L2BlockRef, OutputAtBlock, SyncStatus,
};

/// Shared type alias for the L1 HTTP provider.
///
/// Uses `RootProvider` directly since these clients only perform read operations.
pub type HttpProvider = RootProvider<Ethereum>;

/// L2-specific provider type using the Base network.
///
/// Required for deserializing Base deposit transactions (type 0x7E).
pub type L2HttpProvider = RootProvider<Base>;

/// Base block type with Base-specific transactions.
///
/// Uses `base_common_rpc_types::Transaction` which can deserialize deposit transactions (type 0x7E).
pub type BaseBlock = alloy_rpc_types_eth::Block<base_common_rpc_types::Transaction>;
