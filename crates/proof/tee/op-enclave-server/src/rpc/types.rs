//! RPC request/response types.
//!
//! These types match the Go JSON-RPC request format, using camelCase
//! field names to match go-ethereum's JSON-RPC conventions.

use alloy_primitives::B256;
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

    /// The proposals to aggregate.
    pub proposals: Vec<Proposal>,
}
