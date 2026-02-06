//! RPC request/response types.
//!
//! These types match the Go JSON-RPC request format, using camelCase
//! field names to match go-ethereum's JSON-RPC conventions.

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_primitives::{B256, Bytes};
use serde::{Deserialize, Serialize};

use op_enclave_core::Proposal;
use op_enclave_core::executor::ExecutionWitness;
use op_enclave_core::types::account::AccountResult;
use op_enclave_core::types::config::PerChainConfig;

/// Request for the `executeStateless` RPC method.
///
/// Uses camelCase to match go-ethereum's JSON-RPC conventions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteStatelessRequest {
    /// The per-chain configuration.
    pub config: PerChainConfig,

    /// The L1 origin block header.
    pub l1_origin: Header,

    /// The L1 origin block receipts.
    pub l1_receipts: Vec<ReceiptEnvelope>,

    /// Transactions from the previous L2 block (RLP-encoded).
    pub previous_block_txs: Vec<Bytes>,

    /// The L2 block header to validate.
    pub block_header: Header,

    /// Sequenced transactions for this block (RLP-encoded).
    pub sequenced_txs: Vec<Bytes>,

    /// The execution witness.
    pub witness: ExecutionWitness,

    /// The `L2ToL1MessagePasser` account proof.
    pub message_account: AccountResult,

    /// The storage hash of the message account in the previous block.
    pub prev_message_account_hash: B256,
}

/// Request for the `aggregate` RPC method.
///
/// Uses camelCase to match go-ethereum's JSON-RPC conventions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateRequest {
    /// The per-chain configuration hash.
    pub config_hash: B256,

    /// The output root before the first proposal.
    pub prev_output_root: B256,

    /// The proposals to aggregate.
    pub proposals: Vec<Proposal>,
}
