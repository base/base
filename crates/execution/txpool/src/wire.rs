use alloy_primitives::{Address, Bytes};
use serde::{Deserialize, Serialize};

/// Pre-validated transaction for the builder RPC wire format.
///
/// Carries the recovered sender address so the builder can skip signer
/// recovery, and the EIP-2718 encoded transaction envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedTransaction {
    /// Recovered signer address.
    pub sender: Address,
    /// EIP-2718 encoded transaction bytes.
    pub raw: Bytes,
    /// Target block number for bundle inclusion.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub target_block_number: Option<u64>,
    /// Milliseconds since Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min_timestamp: Option<u64>,
    /// Milliseconds since Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_timestamp: Option<u64>,
}
