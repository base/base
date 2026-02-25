//! Metering response types for bundle simulation.

use alloy_primitives::{Address, B256, TxHash, U256};
use serde::{Deserialize, Serialize};

/// Result of simulating a single transaction within a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
}

/// Response from simulating a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeterBundleResponse {
    /// Effective gas price of the bundle.
    pub bundle_gas_price: U256,
    /// Hash of the bundle (keccak256 of concatenated tx hashes).
    pub bundle_hash: B256,
    /// Total change in coinbase balance.
    pub coinbase_diff: U256,
    /// Total ETH sent directly to coinbase.
    pub eth_sent_to_coinbase: U256,
    /// Total gas fees paid.
    pub gas_fees: U256,
    /// Results for each transaction in the bundle.
    pub results: Vec<TransactionResult>,
    /// Block number used for simulation state.
    pub state_block_number: u64,
    /// Flashblock index used for simulation state.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_flashblock_index: Option<u64>,
    /// Total gas used by all transactions.
    pub total_gas_used: u64,
    /// Total execution time in microseconds.
    pub total_execution_time_us: u128,
    /// Time spent calculating state root in microseconds.
    #[serde(default)]
    pub state_root_time_us: u128,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn test_transaction_result_serialization() {
        let result = TransactionResult {
            coinbase_diff: U256::from(100),
            eth_sent_to_coinbase: U256::from(0),
            from_address: address!("0x1111111111111111111111111111111111111111"),
            gas_fees: U256::from(21000),
            gas_price: U256::from(1_000_000_000),
            gas_used: 21000,
            to_address: Some(address!("0x2222222222222222222222222222222222222222")),
            tx_hash: B256::default(),
            value: U256::from(1_000_000_000_000_000_000u64),
            execution_time_us: 500,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"fromAddress\":\"0x1111111111111111111111111111111111111111\""));
        assert!(json.contains("\"toAddress\":\"0x2222222222222222222222222222222222222222\""));
        assert!(json.contains("\"gasUsed\":21000"));

        let deserialized: TransactionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, result);
    }

    #[test]
    fn test_transaction_result_contract_creation() {
        let result = TransactionResult {
            coinbase_diff: U256::from(100),
            eth_sent_to_coinbase: U256::from(0),
            from_address: address!("0x1111111111111111111111111111111111111111"),
            gas_fees: U256::from(100000),
            gas_price: U256::from(1_000_000_000),
            gas_used: 100000,
            to_address: None,
            tx_hash: B256::default(),
            value: U256::ZERO,
            execution_time_us: 1000,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"toAddress\":null"));

        let deserialized: TransactionResult = serde_json::from_str(&json).unwrap();
        assert!(deserialized.to_address.is_none());
    }

    #[test]
    fn test_meter_bundle_response_default() {
        let response = MeterBundleResponse::default();
        assert_eq!(response.bundle_gas_price, U256::ZERO);
        assert_eq!(response.coinbase_diff, U256::ZERO);
        assert!(response.results.is_empty());
        assert_eq!(response.state_block_number, 0);
        assert!(response.state_flashblock_index.is_none());
        assert_eq!(response.total_gas_used, 0);
    }

    #[test]
    fn test_meter_bundle_response_serialization() {
        let response = MeterBundleResponse {
            bundle_gas_price: U256::from(1000000000),
            bundle_hash: B256::default(),
            coinbase_diff: U256::from(100),
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: U256::from(100),
            results: vec![],
            state_block_number: 12345,
            state_flashblock_index: Some(42),
            total_gas_used: 21000,
            total_execution_time_us: 1000,
            state_root_time_us: 500,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"stateFlashblockIndex\":42"));
        assert!(json.contains("\"stateBlockNumber\":12345"));
        assert!(json.contains("\"stateRootTimeUs\":500"));

        let deserialized: MeterBundleResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.state_flashblock_index, Some(42));
        assert_eq!(deserialized.state_block_number, 12345);
    }

    #[test]
    fn test_meter_bundle_response_without_flashblock_index() {
        let response = MeterBundleResponse {
            bundle_gas_price: U256::from(1000000000),
            bundle_hash: B256::default(),
            coinbase_diff: U256::from(100),
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: U256::from(100),
            results: vec![],
            state_block_number: 12345,
            state_flashblock_index: None,
            total_gas_used: 21000,
            total_execution_time_us: 1000,
            state_root_time_us: 0,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("stateFlashblockIndex"));
        assert!(json.contains("\"stateBlockNumber\":12345"));

        let deserialized: MeterBundleResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.state_flashblock_index, None);
        assert_eq!(deserialized.state_block_number, 12345);
    }

    #[test]
    fn test_meter_bundle_response_deserialization_without_flashblock() {
        let json = r#"{
            "bundleGasPrice": "1000000000",
            "bundleHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "coinbaseDiff": "100",
            "ethSentToCoinbase": "0",
            "gasFees": "100",
            "results": [],
            "stateBlockNumber": 12345,
            "totalGasUsed": 21000,
            "totalExecutionTimeUs": 1000,
            "stateRootTimeUs": 500
        }"#;

        let deserialized: MeterBundleResponse = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.bundle_gas_price, U256::from(1000000000));
        assert_eq!(deserialized.coinbase_diff, U256::from(100));
        assert_eq!(deserialized.eth_sent_to_coinbase, U256::from(0));
        assert_eq!(deserialized.state_flashblock_index, None);
        assert_eq!(deserialized.state_block_number, 12345);
        assert_eq!(deserialized.total_gas_used, 21000);
    }
}
