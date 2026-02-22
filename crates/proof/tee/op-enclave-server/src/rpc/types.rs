//! RPC request/response types.
//!
//! These types match the Go JSON-RPC request format, using camelCase
//! field names to match go-ethereum's JSON-RPC conventions.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

use op_enclave_core::Proposal;

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

    /// The L2 block number before the first proposal in the batch.
    pub prev_block_number: u64,

    /// The proposals to aggregate.
    pub proposals: Vec<Proposal>,

    /// The proposer address included in the signed journal.
    pub proposer: Address,

    /// The keccak256 hash of the TEE image PCR0.
    pub tee_image_hash: B256,

    /// Intermediate output roots at every `intermediate_block_interval` blocks.
    ///
    /// Empty for individual block proofs; populated for aggregated proposals.
    #[serde(default)]
    pub intermediate_roots: Vec<B256>,
}
