//! Subscription types for the `eth_` `PubSub` RPC extension.
//!
//! These types are the serialized shape consumers of `eth_subscribe` actually
//! observe over the wire. Stability invariants:
//!
//! - **Patch releases** (`v0.X.Y` → `v0.X.Y+1`) never change field names or types,
//!   never reorder serde renames, and never remove fields.
//! - **Minor releases** (`v0.X.Y` → `v0.X+1.0`) may add OPTIONAL fields, but
//!   never re-type or remove existing ones. Subscribers can ignore unknown fields
//!   without misinterpreting payloads.
//! - **Major releases** are the only place existing fields may be removed or
//!   re-shaped. Each major release publishes a migration note in the operator
//!   docs (`base-v1-upgrade.mdx`) calling out exactly which payload shapes
//!   changed and what consumers need to do.
//!
//! Consumers caching flashblock state across reconnects should additionally
//! verify the [`Flashblock`]-level `parent_hash` against the parent hash of
//! their last cached pending block. Reorgs and sequencer failovers can produce
//! two different parent hashes at the same block number, and the subscription
//! does NOT explicitly signal these — see the cache invalidation discussion
//! in the architecture page.

use alloy_consensus::Eip658Value;
use alloy_primitives::{Address, Bloom};
use alloy_rpc_types_eth::{Log, pubsub::SubscriptionKind};
use base_common_rpc_types::Transaction;
use derive_more::From;
use serde::{Deserialize, Serialize};

/// A full transaction object with its associated logs and receipt-equivalent fields.
///
/// Returned by the `newFlashblockTransactions` subscription when the second
/// `eth_subscribe` parameter is either `true` or a log filter object. Combines
/// the standard [`Transaction`] shape with the per-transaction logs emitted by
/// its execution AND a subset of receipt fields (`gasUsed`, `status`,
/// `cumulativeGasUsed`, `contractAddress`, `logsBloom`).
///
/// # Receipt-field provenance
///
/// The receipt-shaped fields here come from the flashblock executor running
/// the transaction during pre-confirmation — they are NOT fetched from the
/// final block's receipt store, because the final block has not yet been
/// sealed when this notification is emitted. Once the block IS sealed, the
/// corresponding receipt available via `eth_getTransactionReceipt` will have
/// identical `gasUsed` / `status` / `cumulativeGasUsed` / `contractAddress` /
/// `logsBloom` for the same transaction hash. Consumers should NOT see drift
/// between the pre-confirmed and the sealed values.
///
/// # Stability
///
/// The serialized shape (camelCase field names) is stable across patch and
/// minor releases. New optional fields may be added in a minor release;
/// consumers should ignore unknown fields. Field removals or type changes
/// only happen in major releases and are called out in the upgrade guide.
///
/// # Ordering
///
/// Within a single Flashblock, the `newFlashblockTransactions` subscription
/// emits one notification per transaction, in the order the transactions
/// were included in that Flashblock. Across Flashblocks, ordering matches
/// the Flashblock arrival order — which is the same order in which the
/// transactions will appear in the sealed block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionWithLogs {
    /// The full transaction object. The same shape returned by
    /// `eth_getTransactionByHash` for a finalized transaction, except
    /// `blockHash` is always `null` here because the block is not yet sealed
    /// when the notification fires.
    #[serde(flatten)]
    pub transaction: Transaction,
    /// Logs emitted by this transaction during its flashblock execution.
    ///
    /// Each [`Log`] is the standard Ethereum log shape: `address`, `topics`,
    /// `data`, plus the block/transaction context fields. `blockHash` is set
    /// to the partial-block hash at time of pre-confirmation — this is a
    /// flashblock hash, not the eventual sealed-block hash, and consumers
    /// must not treat it as canonical until the block is sealed. `removed`
    /// is always `false`; flashblock logs do not currently surface reorg
    /// removals via this subscription (a future minor release may add this).
    pub logs: Vec<Log>,
    /// Gas consumed by this transaction's execution, in wei units of gas
    /// (i.e. a count, not a price). Encoded as a hex quantity per JSON-RPC
    /// convention. Required field as of v0.8.0 — earlier versions returned
    /// an optional `Option<u64>` here and consumers had to handle `null`.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// EIP-658 transaction status: `0x1` on success, `0x0` on revert.
    /// Flattened into the parent object using the same convention as
    /// `eth_getTransactionReceipt` so existing receipt-parsing consumers can
    /// reuse their deserializers.
    #[serde(flatten)]
    pub status: Eip658Value,
    /// Cumulative gas used in the (in-progress) block up to and including
    /// this transaction. Strictly monotonic across transactions within a
    /// block; consumers can use the delta between consecutive transactions
    /// as an alternative to reading `gas_used`.
    #[serde(with = "alloy_serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Address of the deployed contract for contract-creation transactions
    /// (`to == null`). `None` for all other transactions. The address is
    /// computed deterministically from `(sender, nonce)` per EIP-1014, so
    /// consumers can verify or pre-compute it before the receipt arrives.
    pub contract_address: Option<Address>,
    /// Bloom filter for all logs emitted by this transaction. Useful for
    /// fast log-filter pre-checks: if the bloom does not contain a topic the
    /// consumer cares about, the consumer can skip scanning `logs` entirely.
    /// Computed identically to `eth_getTransactionReceipt.logsBloom` so the
    /// pre-confirmed and sealed values match for the same transaction hash.
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
///
/// All three subscriptions emit one notification per WebSocket frame and
/// arrive at the ~200ms flashblock cadence. None of them backfill — consumers
/// that disconnect and reconnect will not receive notifications for flashblocks
/// that arrived during the disconnect window. To recover state, query the
/// pending block via standard JSON-RPC after reconnecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BaseSubscriptionKind {
    /// New flashblocks subscription.
    ///
    /// Fires a notification each time a new flashblock is processed, providing
    /// the current pending block state. Each flashblock represents an
    /// incremental update to the pending block, so multiple notifications may
    /// be emitted for the same block height as new flashblocks arrive.
    ///
    /// # Payload shape
    ///
    /// Each notification's `result` is a **standard Ethereum block object**
    /// in the same shape as `eth_getBlockByNumber` with `hydrated=true`. The
    /// transactions array is always hydrated — flashblock subscriptions never
    /// return transaction hashes only.
    ///
    /// Key differences from a sealed block:
    /// - `hash`, `stateRoot`, `receiptsRoot`, `transactionsRoot` reflect the
    ///   partial state at time of emission and will differ from the final
    ///   sealed values.
    /// - The same `number` will appear in multiple notifications as more
    ///   flashblocks accumulate. Consumers should treat each notification as
    ///   the latest known state of the in-progress block — NOT as a diff
    ///   against the previous one.
    ///
    /// # Cache invalidation
    ///
    /// Consumers caching simulation or execution results across flashblocks
    /// MUST verify the incoming `parent_hash` matches the parent_hash of
    /// their cached pending block. A mismatch indicates a reorg or sequencer
    /// failover and the cache must be discarded — two different parent
    /// hashes can share the same block number across these events.
    NewFlashblocks,
    /// Pending logs subscription.
    ///
    /// Returns logs from flashblocks pending state that match the given
    /// filter criteria. Unlike the standard `logs` subscription, which only
    /// includes logs from confirmed blocks, this includes logs from the
    /// current pending flashblock state.
    ///
    /// # Payload shape
    ///
    /// Each notification's `result` is a single [`Log`] object — the standard
    /// Ethereum log shape (`address`, `topics`, `data`, plus block/transaction
    /// context). `blockHash` is always `0x000...000` (all zeros) for pending
    /// logs because the block hash is not yet known. `removed` is always
    /// `false`; pending logs do not surface reorg removals through this
    /// subscription.
    ///
    /// # Filter semantics
    ///
    /// `address` and `topics` use identical matching semantics to
    /// `eth_getLogs` and the standard `logs` subscription. An omitted filter
    /// returns all logs from all pending transactions.
    PendingLogs,
    /// New flashblock transactions subscription.
    ///
    /// Returns transactions from flashblocks as they are sequenced, providing
    /// higher inclusion confidence than standard `newPendingTransactions`
    /// (which returns mempool transactions that may never be sequenced).
    /// Flashblock transactions have been included by the sequencer and are
    /// effectively pre-confirmed for the in-progress block.
    ///
    /// # Modes
    ///
    /// The second `eth_subscribe` parameter controls payload content:
    /// - `true` — Returns full [`TransactionWithLogs`] objects: transaction
    ///   fields + logs + receipt-shaped fields (`gasUsed`, `status`,
    ///   `cumulativeGasUsed`, `contractAddress`, `logsBloom`).
    /// - `false` (default) — Returns transaction hash strings only. Consumers
    ///   that want details should follow up with `eth_getTransactionByHash`
    ///   on the same Flashblocks endpoint.
    /// - A log-filter object (`{ "address": "0x…", "topics": [...] }`) —
    ///   Returns full [`TransactionWithLogs`] for transactions where at
    ///   least one emitted log matches the filter. All logs of the matching
    ///   transaction are included, not just the matching ones.
    ///
    /// # Ordering and dedup
    ///
    /// Per-flashblock ordering matches inclusion order. Transactions are not
    /// duplicated across flashblocks — once a transaction has been emitted
    /// in one Flashblock notification, it will not be emitted again for the
    /// same block (even if a later flashblock includes additional
    /// transactions in the same block).
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
