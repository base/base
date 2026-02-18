//! RPC request types shared between client and server.

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_primitives::{Address, B256, Bytes};
use kona_genesis::ChainConfig;
use serde::{Deserialize, Serialize};

use crate::executor::ExecutionWitness;
use crate::types::account::AccountResult;

/// Request for the `executeStateless` RPC method.
///
/// Uses camelCase to match go-ethereum's JSON-RPC conventions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteStatelessRequest {
    /// The full kona chain configuration.
    pub config: ChainConfig,

    /// The per-chain configuration hash used in proposal signing.
    pub config_hash: B256,

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

    /// The proposer address included in the signed journal for on-chain verification.
    pub proposer: Address,

    /// The keccak256 hash of the TEE image PCR0, included in the signed journal.
    pub tee_image_hash: B256,
}
