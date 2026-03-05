use alloy_primitives::{Address, Bytes};
use serde::{Deserialize, Serialize};

/// Pre-validated transaction for the builder RPC wire format.
///
/// Carries the recovered sender address so the builder can skip signer
/// recovery, and the EIP-2718 encoded transaction envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidTransaction {
    /// Recovered signer address.
    pub sender: Address,
    /// EIP-2718 encoded transaction bytes.
    pub raw: Bytes,
}
