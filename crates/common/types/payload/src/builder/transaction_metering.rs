//! Transaction resource observations.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, TxHash, U256};

/// Per-opcode or precompile gas usage for a single item.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpcodeGas {
    /// Address of the contract/precompile whose execution consumed this gas.
    #[cfg_attr(feature = "serde", serde(default))]
    pub contract_address: Address,
    /// Opcode or precompile name (e.g., "SSTORE", "BLAKE2F").
    pub opcode: String,
    /// Number of times this opcode/precompile was executed in the transaction.
    pub count: u64,
    /// Total gas consumed by all executions of this opcode/precompile.
    pub gas_used: u64,
}

/// Result of simulating a single transaction during execution.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TransactionResult {
    /// Change in coinbase balance after this transaction.
    pub coinbase_diff: U256,
    /// ETH explicitly sent to coinbase (e.g., via direct transfer).
    pub eth_sent_to_coinbase: U256,
    /// Sender address of the transaction.
    pub from_address: Address,
    /// Gas fees paid by this transaction.
    pub gas_fees: U256,
    /// Gas price of the transaction.
    pub gas_price: U256,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Recipient address (None for contract creation).
    pub to_address: Option<Address>,
    /// Hash of the transaction.
    pub tx_hash: TxHash,
    /// Value transferred in the transaction.
    pub value: U256,
    /// Time spent executing this transaction in microseconds.
    pub execution_time_us: u128,
    /// Per-opcode and precompile gas usage for this transaction.
    /// Only populated when opcode metering is enabled.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub opcode_gas: Vec<OpcodeGas>,
}
