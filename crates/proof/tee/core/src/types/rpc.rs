//! RPC request types shared between client and server.

use alloy_primitives::{Address, B256};
use base_proof_preimage::PreimageKey;
use serde::{Deserialize, Serialize};

use crate::Proposal;

/// Request for the `executeStateless` RPC method.
///
/// The `preimages` list contains all data needed for derivation and execution:
/// - Local keys (0-7): `BootInfo` fields (`l1_head`, `agreed_l2_output_root`, configs, etc.)
/// - Keccak keys: L1 block headers, receipts, transactions, blobs
/// - Keccak keys: L2 block headers, transactions, state trie nodes, contract code
///
/// Keccak-keyed preimages are self-verifying: key == keccak256(value).
/// Local-keyed preimages (`BootInfo`) are verified by on-chain contract state.
///
/// Uses camelCase to match go-ethereum's JSON-RPC conventions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteStatelessRequest {
    /// Pre-populated preimage store for the oracle.
    ///
    /// Encoded as a list of `(key, value)` pairs rather than a map because
    /// [`PreimageKey`] is a struct and JSON object keys must be strings.
    /// Convert to a `HashMap` on receipt via `preimages.into_iter().collect()`.
    pub preimages: Vec<(PreimageKey, Vec<u8>)>,

    /// Address of the proposer submitting this block.
    pub proposer: Address,

    /// Hash of the TEE image for attestation verification.
    pub tee_image_hash: B256,
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
