//! RPC response types and provider aliases for Base proof clients.

use alloy_network::Ethereum;
use alloy_provider::RootProvider;
use base_common_network::Base;

/// Shared type alias for the L1 HTTP provider.
///
/// Uses `RootProvider` directly since these clients only perform read operations.
pub type HttpProvider = RootProvider<Ethereum>;

/// L2-specific provider type using the Base network.
///
/// Required for deserializing Base deposit transactions (type 0x7E).
pub type L2HttpProvider = RootProvider<Base>;

/// Base header type with Base-specific optional timestamp extensions.
pub type BaseHeader = base_common_rpc_types::BaseHeaderResponse<alloy_rpc_types_eth::Header>;

/// Base block type with Base-specific transactions.
///
/// Uses `base_common_rpc_types::Transaction` which can deserialize deposit transactions (type 0x7E).
pub type BaseBlock = base_common_rpc_types::BaseBlockResponse<base_common_rpc_types::Transaction>;
