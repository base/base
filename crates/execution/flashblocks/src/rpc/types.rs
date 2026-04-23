//! Subscription types for the `eth_` `PubSub` RPC extension

use alloy_consensus::Eip658Value;
use alloy_primitives::{Address, Bloom};
use alloy_rpc_types_eth::{Log, pubsub::SubscriptionKind};
use base_common_rpc_types::Transaction;
use derive_more::From;
use serde::{Deserialize, Serialize};

/// A full transaction object with its associated logs and receipt-equivalent fields.
///
/// This is returned by `newFlashblockTransactions` subscription when `full = true`
/// or when a log filter is provided, giving both the transaction details, logs emitted
/// by its execution, and receipt-derived fields already available from flashblock execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionWithLogs {
    /// The full transaction object.
    #[serde(flatten)]
    pub transaction: Transaction,
    /// Logs emitted by this transaction.
    pub logs: Vec<Log>,
    /// Gas consumed by this transaction's execution.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// Status of the transaction, serialized the same way as `eth_getTransactionReceipt`.
    #[serde(flatten)]
    pub status: Eip658Value,
    /// Cumulative gas used in the block up to and including this transaction.
    #[serde(with = "alloy_serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Contract address created, if this was a contract creation transaction.
    pub contract_address: Option<Address>,
    /// Bloom filter for all logs emitted by this transaction.
    pub logs_bloom: Bloom,
}

/// Extended subscription kind that includes both standard Ethereum subscription types
/// and flashblocks-specific types.
///
/// This enum encapsulates the standard [`SubscriptionKind`] from alloy and adds flashblocks
/// support, allowing `eth_subscribe` to handle both standard subscriptions (newHeads, logs, etc.)
/// and custom flashblocks subscriptions.
///
/// By encapsulating [`SubscriptionKind`] rather than redefining its variants, we automatically
/// inherit support for any new variants added upstream, or get a compile error if the signature
/// changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, From)]
#[serde(untagged)]
pub enum ExtendedSubscriptionKind {
    /// Standard Ethereum subscription types (newHeads, logs, newPendingTransactions, syncing).
    ///
    /// These are proxied to reth's underlying `EthPubSub` implementation.
    #[from]
    Standard(SubscriptionKind),
    /// Base-specific subscription types for flashblocks.
    #[from]
    Base(BaseSubscriptionKind),
}

/// Base-specific subscription types for flashblocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BaseSubscriptionKind {
    /// New flashblocks subscription.
    ///
    /// Fires a notification each time a new flashblock is processed, providing the current
    /// pending block state. Each flashblock represents an incremental update to the pending
    /// block, so multiple notifications may be emitted for the same block height as new
    /// flashblocks arrive.
    NewFlashblocks,
    /// Pending logs subscription.
    ///
    /// Returns logs from flashblocks pending state that match the given filter criteria.
    /// Unlike standard `logs` subscription which only includes logs from confirmed blocks,
    /// this includes logs from the current pending flashblock state.
    PendingLogs,
    /// New flashblock transactions subscription.
    ///
    /// Returns transactions from flashblocks as they are sequenced, providing higher inclusion
    /// confidence than standard `newPendingTransactions` which returns mempool transactions.
    /// Flashblock transactions have been included by the sequencer and are effectively preconfirmed.
    ///
    /// Accepts an optional parameter:
    /// - `true`: Returns full transaction objects with their associated logs (as
    ///   [`TransactionWithLogs`])
    /// - `false` (default): Returns only transaction hashes
    /// - A log filter object (with `address` and/or `topics`): Returns full transaction objects
    ///   where at least one log matches the filter. All logs are included in the response, not
    ///   just the matching ones.
    NewFlashblockTransactions,
}

impl ExtendedSubscriptionKind {
    /// Returns the standard subscription kind if this is a standard subscription type.
    pub const fn as_standard(&self) -> Option<SubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(*kind),
            Self::Base(_) => None,
        }
    }

    /// Returns true if this is a flashblocks-specific subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::Base(_))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, transaction::Recovered};
    use alloy_primitives::{
        Address, B256, Bytes, Log as PrimitiveLog, LogData, Signature, TxKind, U256,
    };
    use alloy_rpc_types_eth::Log;
    use base_common_consensus::BaseTxEnvelope;
    use base_common_rpc_types::Transaction;

    use super::*;

    fn test_transaction_with_logs() -> TransactionWithLogs {
        let legacy = alloy_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 7,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::with_last_byte(0xBB)),
            value: U256::from(1_000_000u64),
            input: Bytes::new(),
        };
        let hash = B256::with_last_byte(0xAA);
        let envelope = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            legacy,
            Signature::test_signature(),
            hash,
        ));
        let recovered = Recovered::new_unchecked(envelope, Address::with_last_byte(0xCC));
        let tx = Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: recovered,
                block_hash: Some(B256::ZERO),
                block_number: Some(42),
                transaction_index: Some(3),
                effective_gas_price: Some(1_000_000_000),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        };

        let log = Log {
            inner: PrimitiveLog {
                address: Address::with_last_byte(0xDD),
                data: LogData::new_unchecked(
                    vec![B256::with_last_byte(0xEE)],
                    Bytes::from_static(&[0x01, 0x02]),
                ),
            },
            block_hash: Some(B256::ZERO),
            block_number: Some(42),
            block_timestamp: None,
            transaction_hash: Some(hash),
            transaction_index: Some(3),
            log_index: Some(0),
            removed: false,
        };

        TransactionWithLogs {
            transaction: tx,
            logs: vec![log],
            gas_used: 21_000,
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 42_000,
            contract_address: Some(Address::with_last_byte(0xEF)),
            logs_bloom: [0x11; 256].into(),
        }
    }

    #[test]
    fn transaction_with_logs_json_format() {
        let twl = test_transaction_with_logs();
        let json = serde_json::to_value(&twl).expect("serialization should succeed");
        let obj = json.as_object().expect("should be a JSON object");

        assert!(obj.contains_key("logs"), "missing 'logs' field");
        assert!(obj.contains_key("gasUsed"), "missing 'gasUsed' field");
        assert!(obj.contains_key("status"), "missing 'status' field");
        assert!(obj.contains_key("cumulativeGasUsed"), "missing 'cumulativeGasUsed' field");
        assert!(obj.contains_key("contractAddress"), "missing 'contractAddress' field");
        assert!(obj.contains_key("logsBloom"), "missing 'logsBloom' field");
        assert!(obj.contains_key("nonce"), "missing flattened tx 'nonce' field");
        assert!(obj.contains_key("gasPrice"), "missing flattened tx 'gasPrice' field");
        assert!(obj.contains_key("hash"), "missing flattened tx 'hash' field");
        assert!(obj.contains_key("from"), "missing flattened tx 'from' field");
        assert!(obj.contains_key("to"), "missing flattened tx 'to' field");
        assert!(obj.contains_key("value"), "missing flattened tx 'value' field");
        assert!(obj.contains_key("blockNumber"), "missing flattened tx 'blockNumber' field");

        assert_eq!(obj["gasUsed"], "0x5208", "gasUsed should use receipt quantity encoding");
        assert_eq!(obj["status"], "0x1", "status should use receipt quantity encoding");
        assert_eq!(
            obj["cumulativeGasUsed"], "0xa410",
            "cumulativeGasUsed should use receipt quantity encoding"
        );
        assert_eq!(
            obj["contractAddress"],
            format!("{:#x}", Address::with_last_byte(0xEF)),
            "contractAddress should serialize as an address"
        );
        assert_eq!(
            obj["logsBloom"],
            format!("0x{}", "11".repeat(256)),
            "logsBloom should serialize as a bloom hex string"
        );

        let logs = obj["logs"].as_array().expect("logs should be an array");
        assert_eq!(logs.len(), 1);
        let log = logs[0].as_object().expect("log should be a JSON object");
        assert!(log.contains_key("address"), "log missing 'address' field");
        assert!(log.contains_key("topics"), "log missing 'topics' field");
        assert!(log.contains_key("data"), "log missing 'data' field");
        assert!(log.contains_key("transactionHash"), "log missing 'transactionHash' field");
    }

    #[test]
    fn transaction_with_logs_json_roundtrip() {
        let original = test_transaction_with_logs();
        let json_str = serde_json::to_string(&original).expect("serialization should succeed");
        let deserialized: TransactionWithLogs =
            serde_json::from_str(&json_str).expect("deserialization should succeed");

        assert_eq!(original, deserialized);
    }

    #[test]
    fn transaction_with_logs_json_string_contains_expected_fields() {
        let twl = test_transaction_with_logs();
        let json_str = serde_json::to_string(&twl).expect("serialization should succeed");

        assert!(
            json_str.contains("\"gasUsed\":\"0x5208\""),
            "JSON must contain gasUsed key with quantity encoding"
        );
        assert!(json_str.contains("\"status\":\"0x1\""), "JSON must contain status key");
        assert!(
            json_str.contains("\"cumulativeGasUsed\":\"0xa410\""),
            "JSON must contain cumulativeGasUsed key"
        );
        assert!(json_str.contains("\"contractAddress\""), "JSON must contain contractAddress key");
        assert!(json_str.contains("\"logsBloom\""), "JSON must contain logsBloom key");
        assert!(json_str.contains("\"logs\""), "JSON must contain logs key");
        assert!(json_str.contains("\"gasPrice\""), "JSON must contain gasPrice key");
        assert!(json_str.contains("\"nonce\""), "JSON must contain nonce key");
        assert!(json_str.contains("\"hash\""), "JSON must contain hash key");
        assert!(json_str.contains("\"from\""), "JSON must contain from key");
        assert!(json_str.contains("\"to\""), "JSON must contain to key");
        assert!(json_str.contains("\"blockNumber\""), "JSON must contain blockNumber key");
        assert!(json_str.contains("\"topics\""), "JSON must contain topics key in logs");
        assert!(json_str.contains("\"address\""), "JSON must contain address key in logs");
        assert!(
            json_str.contains("\"transactionHash\""),
            "JSON must contain transactionHash key in logs"
        );
    }

    #[test]
    fn transaction_with_logs_contract_address_none_serialization() {
        let mut twl = test_transaction_with_logs();
        twl.contract_address = None;
        let json = serde_json::to_value(&twl).expect("serialization should succeed");
        let obj = json.as_object().expect("should be a JSON object");

        assert!(
            obj.contains_key("contractAddress"),
            "contractAddress key should be present even when None"
        );
        assert!(obj["contractAddress"].is_null(), "contractAddress should be null when None");
        assert_eq!(obj["gasUsed"], "0x5208", "gasUsed should remain a required quantity field");
        assert_eq!(obj["status"], "0x1", "status should remain a required receipt field");
        assert_eq!(
            obj["cumulativeGasUsed"], "0xa410",
            "cumulativeGasUsed should remain a required quantity field"
        );
        assert_eq!(
            obj["logsBloom"],
            format!("0x{}", "11".repeat(256)),
            "logsBloom should remain a required bloom field"
        );
    }
}
