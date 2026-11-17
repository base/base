//! Request and response types of the differential execution protocol (see `PROTOCOL.md`).

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use serde::{Deserialize, Serialize};

/// Environment variable the harness sets on the reference adapter to ask for `coverage` in
/// its responses (see "Optional coverage" in `PROTOCOL.md`).
pub const COVERAGE_ENV: &str = "BASE_DIFF_COVERAGE";

/// Execution mode of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Fuzzed block template: invalid transactions are skipped and reported.
    Build,
    /// Canonical block: imported on the normal validation path.
    Validate,
}

/// Header fields of the block under execution.
///
/// In build mode only the inputs are set; in validate mode every field is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestHeader {
    /// Block number.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,
    /// Block timestamp in seconds.
    #[serde(with = "alloy_serde::quantity")]
    pub timestamp: u64,
    /// Fee recipient.
    pub coinbase: Address,
    /// `prevRandao` (the header's mix hash).
    pub prev_randao: B256,
    /// Block gas limit.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_limit: u64,
    /// Base fee used for execution.
    #[serde(with = "alloy_serde::quantity")]
    pub base_fee_per_gas: u64,
    /// Header extra data (Holocene EIP-1559 parameters, Jovian minimum base fee).
    pub extra_data: Bytes,
    /// Parent beacon block root (Ecotone and later).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_beacon_block_root: Option<B256>,
    /// Excess blob gas (Ecotone and later).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub excess_blob_gas: Option<u64>,
    /// Canonical result fields, set only in validate mode.
    #[serde(flatten)]
    pub results: Option<HeaderResults>,
}

/// Header fields that result from executing a block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderResults {
    /// State root after the block.
    pub state_root: B256,
    /// Receipts root.
    pub receipts_root: B256,
    /// Transactions root.
    pub transactions_root: B256,
    /// Logs bloom.
    pub logs_bloom: Bloom,
    /// Gas used by the block.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// Blob gas used (the DA footprint from Jovian), when the fork has the field.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub blob_gas_used: Option<u64>,
    /// Withdrawals root (the message passer storage root from Isthmus), when the fork has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawals_root: Option<B256>,
    /// Requests hash, when the fork has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_hash: Option<B256>,
    /// Hash of the full header.
    pub hash: B256,
}

/// Stateless pre-state rooted at the parent's state root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// RLP trie node preimages.
    pub state: Vec<Bytes>,
    /// Contract bytecodes.
    pub codes: Vec<Bytes>,
}

/// One execution request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// Case identifier, echoed in the response.
    pub id: String,
    /// Chain configuration name; always `base-mainnet` today.
    pub chain: String,
    /// Execution mode.
    pub mode: Mode,
    /// RLP ancestor headers, newest first; `ancestors[0]` is the parent.
    pub ancestors: Vec<Bytes>,
    /// The block's header fields.
    pub header: RequestHeader,
    /// EIP-2718 encoded transactions in block order.
    pub transactions: Vec<Bytes>,
    /// Parent pre-state witness.
    pub witness: Witness,
    /// Optional EIP-3155 trace output directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
}

/// A transaction skipped in build mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected {
    /// Index into the request's transactions.
    pub index: usize,
    /// Free-text reason; not compared.
    pub error: String,
}

/// A log emitted by a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    /// Emitting contract.
    pub address: Address,
    /// Topics.
    pub topics: Vec<B256>,
    /// Data.
    pub data: Bytes,
}

/// Receipt of an included transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    /// EIP-2718 transaction type.
    #[serde(rename = "type", with = "alloy_serde::quantity")]
    pub tx_type: u8,
    /// 1 for success, 0 for failure.
    #[serde(with = "alloy_serde::quantity")]
    pub status: u8,
    /// Cumulative gas used in the block after this transaction.
    #[serde(with = "alloy_serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Logs.
    pub logs: Vec<Log>,
    /// Deposit nonce (deposits, Regolith and later).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_nonce: Option<u64>,
    /// Deposit receipt version (deposits, Canyon and later).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_receipt_version: Option<u64>,
}

/// Post-block values of an account the block changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostAccount {
    /// Nonce.
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash.
    pub code_hash: B256,
    /// Storage slots whose value changed, with their new values.
    #[serde(default)]
    pub storage: BTreeMap<B256, B256>,
    /// The account was destroyed and recreated in this block (pre-Cancun `SELFDESTRUCT`), so
    /// its storage starts from empty: every parent slot not listed in `storage` is now zero.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wiped: bool,
}

/// One execution response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    /// Case identifier from the request.
    pub id: String,
    /// False only for adapter-level failures.
    pub ok: bool,
    /// Adapter failure description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the block is valid (validate) or could be built (build).
    #[serde(default)]
    pub valid: bool,
    /// Free-text invalidity reason; not compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    /// Skipped transactions (build mode).
    #[serde(default)]
    pub rejected: Vec<Rejected>,
    /// Computed header fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<HeaderResults>,
    /// Base fee implied by the parent and chain rules.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub computed_base_fee_per_gas: Option<u64>,
    /// Receipts of included transactions.
    #[serde(default)]
    pub receipts: Vec<Receipt>,
    /// Changed accounts; `None` marks a deleted account.
    #[serde(default)]
    pub post: BTreeMap<Address, Option<PostAccount>>,
    /// Optional coverage features (name to hit count) of the execution; never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<BTreeMap<String, u64>>,
}
