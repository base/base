//! Request and response types of the differential execution protocol (see `PROTOCOL.md`).

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use base_common_genesis::SystemConfig;
use serde::{Deserialize, Serialize};

/// Environment variable the harness sets on the reference adapter to ask for `coverage` in
/// its responses (see "Optional coverage" in `PROTOCOL.md`).
pub const COVERAGE_ENV: &str = "BASE_DIFF_COVERAGE";

/// Execution mode of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Fuzzed block template: invalid transactions are skipped and reported.
    Build,
    /// Canonical block: imported on the normal validation path.
    Validate,
    /// L1 `ConfigUpdate` logs applied to a system config, as derivation does.
    SystemConfig,
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

/// Stateless pre-state rooted at the parent's state root, or at `root` when set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// RLP trie node preimages.
    pub state: Vec<Bytes>,
    /// Contract bytecodes.
    pub codes: Vec<Bytes>,
    /// Root of a synthetic pre-state that replaces the parent's state root (build mode only),
    /// for mainnet blocks whose real witness is unavailable (see "Synthetic pre-state" in
    /// `PROTOCOL.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<B256>,
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
    /// RLP ancestor headers, newest first; `ancestors[0]` is the parent. Empty in
    /// `systemConfig` mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<Bytes>,
    /// The block's header fields. Absent in `systemConfig` mode.
    #[serde(default, skip_serializing_if = "RequestHeader::is_absent")]
    pub header: RequestHeader,
    /// EIP-2718 encoded transactions in block order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transactions: Vec<Bytes>,
    /// Parent pre-state witness. Absent in `systemConfig` mode.
    #[serde(default, skip_serializing_if = "Witness::is_absent")]
    pub witness: Witness,
    /// Optional EIP-3155 trace output directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// The system config update to apply, in `systemConfig` mode only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_config: Option<SystemConfigInput>,
}

impl RequestHeader {
    /// Whether every field is unset, as in a `systemConfig` request.
    pub fn is_absent(&self) -> bool {
        *self == Self::default()
    }
}

impl Witness {
    /// Whether the witness has no nodes and no codes, as in a `systemConfig` request.
    pub const fn is_absent(&self) -> bool {
        self.state.is_empty() && self.codes.is_empty()
    }
}

/// The L1 system config values that `ConfigUpdate` logs change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemConfigValues {
    /// Batcher address.
    pub batcher_address: Address,
    /// Fee overhead (zeroed by Ecotone gas config updates).
    pub overhead: B256,
    /// Fee scalar (from Ecotone a versioned encoding of the base fee and blob base fee scalars).
    pub scalar: B256,
    /// L2 gas limit.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_limit: u64,
    /// EIP-1559 denominator (Holocene).
    #[serde(with = "alloy_serde::quantity")]
    pub eip1559_denominator: u32,
    /// EIP-1559 elasticity (Holocene).
    #[serde(with = "alloy_serde::quantity")]
    pub eip1559_elasticity: u32,
    /// Operator fee scalar (Isthmus).
    #[serde(with = "alloy_serde::quantity")]
    pub operator_fee_scalar: u32,
    /// Operator fee constant (Isthmus).
    #[serde(with = "alloy_serde::quantity")]
    pub operator_fee_constant: u64,
    /// Minimum base fee (Jovian).
    #[serde(with = "alloy_serde::quantity")]
    pub min_base_fee: u64,
    /// DA footprint gas scalar (Jovian).
    #[serde(with = "alloy_serde::quantity")]
    pub da_footprint_gas_scalar: u16,
}

impl From<&SystemConfig> for SystemConfigValues {
    fn from(config: &SystemConfig) -> Self {
        Self {
            batcher_address: config.batcher_address,
            overhead: config.overhead.into(),
            scalar: config.scalar.into(),
            gas_limit: config.gas_limit,
            eip1559_denominator: config.eip1559_denominator.unwrap_or_default(),
            eip1559_elasticity: config.eip1559_elasticity.unwrap_or_default(),
            operator_fee_scalar: config.operator_fee_scalar.unwrap_or_default(),
            operator_fee_constant: config.operator_fee_constant.unwrap_or_default(),
            min_base_fee: config.min_base_fee.unwrap_or_default(),
            da_footprint_gas_scalar: config.da_footprint_gas_scalar.unwrap_or_default(),
        }
    }
}

impl From<&SystemConfigValues> for SystemConfig {
    fn from(values: &SystemConfigValues) -> Self {
        Self {
            batcher_address: values.batcher_address,
            overhead: values.overhead.into(),
            scalar: values.scalar.into(),
            gas_limit: values.gas_limit,
            base_fee_scalar: None,
            blob_base_fee_scalar: None,
            eip1559_denominator: Some(values.eip1559_denominator),
            eip1559_elasticity: Some(values.eip1559_elasticity),
            operator_fee_scalar: Some(values.operator_fee_scalar),
            operator_fee_constant: Some(values.operator_fee_constant),
            min_base_fee: Some(values.min_base_fee),
            da_footprint_gas_scalar: Some(values.da_footprint_gas_scalar),
        }
    }
}

/// The receipt of an L1 transaction, reduced to what system config derivation reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct L1Receipt {
    /// 1 for success, 0 for failure.
    #[serde(with = "alloy_serde::quantity")]
    pub status: u8,
    /// Logs.
    pub logs: Vec<Log>,
}

impl From<&L1Receipt> for alloy_consensus::Receipt {
    fn from(receipt: &L1Receipt) -> Self {
        Self {
            status: alloy_consensus::Eip658Value::Eip658(receipt.status == 1),
            cumulative_gas_used: 0,
            logs: receipt
                .logs
                .iter()
                .map(|log| {
                    alloy_primitives::Log::new_unchecked(
                        log.address,
                        log.topics.clone(),
                        log.data.clone(),
                    )
                })
                .collect(),
        }
    }
}

/// Input of a `systemConfig` request: a system config and the receipts of one L1 block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemConfigInput {
    /// Timestamp of the L1 block; the client decides which forks are active from it with its
    /// Base mainnet rollup configuration, as its derivation does.
    #[serde(with = "alloy_serde::quantity")]
    pub l1_timestamp: u64,
    /// The Base upgrades active at `l1_timestamp`, oldest first (`regolith`, `canyon`,
    /// `ecotone`, ...); derived from `l1_timestamp`, for adapters that pass fork flags.
    pub forks: Vec<String>,
    /// The system config before the block.
    pub config: SystemConfigValues,
    /// The L1 block's receipts, in block order.
    pub receipts: Vec<L1Receipt>,
}

/// What a client did with one log of a `systemConfig` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigLogOutcome {
    /// A well-formed update of a known kind (it may leave the config unchanged).
    Applied,
    /// A `ConfigUpdate` log from the system config address that the client skipped as malformed.
    Rejected,
    /// Not a config update: a failed receipt, another address, or another event.
    Ignored,
    /// The client stopped processing the block's updates at this log (derivation halts).
    Halted,
}

/// The outcome of one log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigLogResult {
    /// What the client did with the log.
    pub outcome: ConfigLogOutcome,
    /// The update kind, for applied logs.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub kind: Option<u64>,
    /// Free-text reason for rejected or halted logs; not compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of a `systemConfig` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemConfigOutput {
    /// The system config after every log; absent when the client halted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SystemConfigValues>,
    /// One result per log of every receipt, in order, ending at a halted log.
    pub logs: Vec<ConfigLogResult>,
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
    /// Base fee implied by the parent and chain rules. Up to 256 bits, so a client whose
    /// computation leaves the `u64` range is reported as a difference, not a bad response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_base_fee_per_gas: Option<U256>,
    /// Receipts of included transactions.
    #[serde(default)]
    pub receipts: Vec<Receipt>,
    /// Changed accounts; `None` marks a deleted account.
    #[serde(default)]
    pub post: BTreeMap<Address, Option<PostAccount>>,
    /// Optional coverage features (name to hit count) of the execution; never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<BTreeMap<String, u64>>,
    /// The updated system config, in `systemConfig` mode only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_config: Option<SystemConfigOutput>,
}
