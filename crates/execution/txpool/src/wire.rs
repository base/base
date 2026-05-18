use alloy_primitives::{Address, B256, Bytes, U256};
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
    /// EIP-8130 Account Abstraction metadata, present when the forwarding
    /// node has already validated the AA transaction. Allows the receiving
    /// builder to skip expensive custom verifier execution and re-use
    /// pre-computed invalidation keys.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub aa_metadata: Option<Eip8130WireMetadata>,
}

/// Pre-validated EIP-8130 metadata forwarded alongside the raw transaction.
///
/// All fields are produced by `validate_eip8130_transaction` on the
/// sequencer's mempool node and forwarded to the builder so it can skip
/// re-deriving them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Eip8130WireMetadata {
    /// The transaction's `nonce_key` (2D nonce lane identifier).
    pub nonce_key: U256,
    /// The sender's current nonce sequence at validation time.
    pub nonce_sequence: u64,
    /// Resolved payer address (`None` for self-pay transactions).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub payer: Option<Address>,
    /// Storage slot dependencies for invalidation tracking.
    /// Each entry is `(contract_address, storage_slot)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalidation_slots: Vec<(Address, B256)>,
    /// Whether the sender's custom verifier execution succeeded.
    pub verifier_passed: bool,
    /// Unix timestamp after which this transaction is invalid. `0` = no expiry.
    #[serde(default)]
    pub expiry: u64,
}
