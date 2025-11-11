//! Flashblock metadata types.

use alloc::collections::BTreeMap;
use alloy_primitives::{Address, B256, U256};
use op_alloy_consensus::OpReceipt;

/// Provides metadata about the block that may be useful for indexing or analysis.
// Note: this uses mixed camel, snake case: <https://github.com/flashbots/rollup-boost/blob/dd12e8e8366004b4758bfa0cfa98efa6929b7e9f/crates/flashblocks-rpc/src/cache.rs#L31>
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpFlashblockPayloadMetadata {
    /// The number of the block in the L2 chain.
    pub block_number: u64,
    /// A map of addresses to their updated balances after the block execution.
    /// This represents balance changes due to transactions, rewards, or system transfers.
    pub new_account_balances: BTreeMap<Address, U256>,
    /// Execution receipts for all transactions in the block.
    /// Contains logs, gas usage, and other EVM-level metadata.
    pub receipts: BTreeMap<B256, OpReceipt>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use alloy_consensus::{Eip658Value, Receipt};
    use alloy_primitives::{Log, address};

    fn sample_metadata() -> OpFlashblockPayloadMetadata {
        let mut balances = BTreeMap::new();
        balances.insert(address!("0000000000000000000000000000000000000001"), U256::from(1000));

        let mut receipts = BTreeMap::new();
        let receipt = OpReceipt::Legacy(Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 21000,
            logs: Vec::new(),
        });
        receipts.insert(B256::ZERO, receipt);

        OpFlashblockPayloadMetadata { block_number: 100, new_account_balances: balances, receipts }
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_metadata_serde_roundtrip() {
        let metadata = sample_metadata();

        let json = serde_json::to_string(&metadata).unwrap();
        let decoded: OpFlashblockPayloadMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(metadata, decoded);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_metadata_snake_case_serialization() {
        let metadata = sample_metadata();

        let json = serde_json::to_string(&metadata).unwrap();
        assert!(json.contains("block_number"));
        assert!(json.contains("new_account_balances"));
        assert!(json.contains("receipts"));
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_address_balance_map_serialization() {
        let mut balances = BTreeMap::new();
        balances.insert(address!("0000000000000000000000000000000000000001"), U256::from(1000));
        balances.insert(address!("0000000000000000000000000000000000000002"), U256::from(2000));

        let metadata = OpFlashblockPayloadMetadata {
            block_number: 1,
            new_account_balances: balances,
            receipts: BTreeMap::new(),
        };

        let json = serde_json::to_value(&metadata).unwrap();
        let balances_obj = json.get("new_account_balances").unwrap();

        // Should be serialized as an object with hex string keys
        assert!(balances_obj.is_object());
        assert!(balances_obj.get("0x0000000000000000000000000000000000000001").is_some());
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_receipt_map_serialization() {
        let mut receipts = BTreeMap::new();
        let receipt1 = OpReceipt::Legacy(Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 21000,
            logs: Vec::<Log>::new(),
        });
        receipts.insert(B256::ZERO, receipt1);

        let metadata = OpFlashblockPayloadMetadata {
            block_number: 1,
            new_account_balances: BTreeMap::new(),
            receipts,
        };

        let json = serde_json::to_value(&metadata).unwrap();
        let receipts_obj = json.get("receipts").unwrap();

        // Should be serialized as an object with hex string keys
        assert!(receipts_obj.is_object());
        assert!(
            receipts_obj
                .get("0x0000000000000000000000000000000000000000000000000000000000000000")
                .is_some()
        );
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_receipt_json_format() {
        let mut receipts = BTreeMap::new();
        let receipt = OpReceipt::Legacy(Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 21000,
            logs: Vec::<Log>::new(),
        });
        receipts.insert(B256::ZERO, receipt);

        let metadata = OpFlashblockPayloadMetadata {
            block_number: 1,
            new_account_balances: BTreeMap::new(),
            receipts,
        };

        let json = serde_json::to_value(&metadata).unwrap();
        let receipts_obj = json.get("receipts").unwrap();
        let receipt_entry = receipts_obj
            .get("0x0000000000000000000000000000000000000000000000000000000000000000")
            .unwrap();

        // OpReceipt serializes as internally tagged enum
        assert!(receipt_entry.get("Legacy").is_some());
    }

    #[test]
    fn test_metadata_default() {
        let metadata = OpFlashblockPayloadMetadata::default();
        assert_eq!(metadata.block_number, 0);
        assert!(metadata.new_account_balances.is_empty());
        assert!(metadata.receipts.is_empty());
    }
}
