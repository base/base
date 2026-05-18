use std::{fmt, sync::Arc};

use alloy_primitives::B256;
use base_proof_succinct_client_utils::precompiles::cycle_tracker::keys;
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Deserializer, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use sp1_sdk::{
    ExecutionReport, NetworkProver, SP1ProofMode, SP1ProvingKey, SP1VerifyingKey,
    network::{FulfillmentStrategy, proto::types::ProofRequest},
};

/// Request to validate an on-chain contract configuration.
#[derive(Serialize, Deserialize, Debug)]
pub struct ValidateConfigRequest {
    /// Contract address to validate.
    pub address: String,
}

/// Response from validating an on-chain contract configuration.
#[derive(Serialize, Deserialize, Debug)]
pub struct ValidateConfigResponse {
    /// Whether the rollup config hash matches.
    pub rollup_config_hash_valid: bool,
    /// Whether the aggregation verification key matches.
    pub agg_vkey_valid: bool,
    /// Whether the range verification key matches.
    pub range_vkey_valid: bool,
}

/// Request body for a span (range) proof.
#[derive(Deserialize, Serialize, Debug)]
pub struct SpanProofRequest {
    /// Start L2 block number.
    pub start: u64,
    /// End L2 block number.
    pub end: u64,
}

/// Request body for an aggregation proof.
#[derive(Deserialize, Serialize, Debug)]
pub struct AggProofRequest {
    /// Serialized subproof byte arrays (base64-encoded in JSON).
    #[serde(deserialize_with = "deserialize_base64_vec")]
    pub subproofs: Vec<Vec<u8>>,
    /// L1 head block hash.
    pub head: String,
}

/// Response from a mock proof request.
#[derive(Deserialize, Serialize, Debug)]
pub struct MockProofResponse {
    /// Identifier of the generated mock proof.
    pub proof_id: String,
}

/// Response from a proof request.
#[derive(Serialize, Deserialize, Debug)]
pub struct ProofResponse {
    /// Proof request identifier bytes.
    pub proof_id: Vec<u8>,
}

#[derive(Debug, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
/// The type of error that occurred when unclaiming a proof. Based off of the `unclaim_description`
/// field in the `ProofStatus` struct.
pub enum UnclaimDescription {
    /// The prover encountered an unexpected internal error.
    UnexpectedProverError = 0,
    /// The proving program failed during execution.
    ProgramExecutionError = 1,
    /// The program exceeded the allowed cycle limit.
    CycleLimitExceeded = 2,
    /// Any other unclaim reason not covered above.
    Other = 3,
}

/// Convert a string to an `UnclaimDescription`. These cover the common reasons why a proof might
/// be unclaimed.
impl From<String> for UnclaimDescription {
    fn from(description: String) -> Self {
        match description.as_str().to_lowercase().as_str() {
            "unexpected prover error" => Self::UnexpectedProverError,
            "program execution error" => Self::ProgramExecutionError,
            "cycle limit exceeded" => Self::CycleLimitExceeded,
            _ => Self::Other,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
/// The status of a proof request.
pub struct ProofStatus {
    // Note: Can't use `FulfillmentStatus`/`ExecutionStatus` directly because `Serialize_repr` and
    // `Deserialize_repr` aren't derived on it.
    /// Numeric fulfillment status from the prover network.
    pub fulfillment_status: i32,
    /// Numeric execution status from the prover network.
    pub execution_status: i32,
    /// Raw proof bytes.
    pub proof: Vec<u8>,
}

/// Configuration of the L2 Output Oracle contract. Created once at server start-up, monitors if
/// there are any changes to the contract's configuration.
/// Full proposer configuration including keys, commitments, and prover settings.
#[derive(Clone)]
pub struct SuccinctProposerConfig {
    /// Range program verifying key.
    pub range_vk: Arc<SP1VerifyingKey>,
    /// Range program proving key.
    pub range_pk: Arc<SP1ProvingKey>,
    /// Aggregation program proving key.
    pub agg_pk: Arc<SP1ProvingKey>,
    /// Aggregation program verifying key.
    pub agg_vk: Arc<SP1VerifyingKey>,
    /// Hash of the aggregation verification key.
    pub agg_vkey_hash: B256,
    /// Commitment to the range verification key.
    pub range_vkey_commitment: B256,
    /// Hash of the rollup configuration.
    pub rollup_config_hash: B256,
    /// Fulfillment strategy for range proofs.
    pub range_proof_strategy: FulfillmentStrategy,
    /// Fulfillment strategy for aggregation proofs.
    pub agg_proof_strategy: FulfillmentStrategy,
    /// SP1 proof mode for aggregation proofs.
    pub agg_proof_mode: SP1ProofMode,
    /// Network prover client.
    pub network_prover: Arc<NetworkProver>,
}

impl fmt::Debug for SuccinctProposerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccinctProposerConfig")
            .field("agg_vkey_hash", &self.agg_vkey_hash)
            .field("range_vkey_commitment", &self.range_vkey_commitment)
            .field("rollup_config_hash", &self.rollup_config_hash)
            .field("range_proof_strategy", &self.range_proof_strategy)
            .field("agg_proof_strategy", &self.agg_proof_strategy)
            .field("agg_proof_mode", &self.agg_proof_mode)
            .finish_non_exhaustive()
    }
}

/// Deserialize a vector of base64 strings into a vector of vectors of bytes. Go serializes
/// the subproofs as base64 strings.
fn deserialize_base64_vec<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Vec<String> = Deserialize::deserialize(deserializer)?;
    s.into_iter()
        .map(|base64_str| {
            general_purpose::STANDARD.decode(base64_str).map_err(serde::de::Error::custom)
        })
        .collect()
}

/// Cycle-level execution statistics from a proof request.
#[derive(Serialize, Debug)]
pub struct RequestExecutionStatistics {
    /// Total instruction cycles executed.
    pub total_instruction_cycles: u64,
    /// Total SP1 gas consumed.
    pub total_sp1_gas: u64,
    /// Cycles spent on block execution.
    pub block_execution_cycles: u64,
    /// Cycles spent on oracle verification.
    pub oracle_verify_cycles: u64,
    /// Cycles spent on payload derivation.
    pub derivation_cycles: u64,
    /// Cycles spent on blob verification.
    pub blob_verification_cycles: u64,
    /// Cycles spent on BN254 point addition precompile.
    pub bn_add_cycles: u64,
    /// Cycles spent on BN254 scalar multiplication precompile.
    pub bn_mul_cycles: u64,
    /// Cycles spent on BN254 pairing precompile.
    pub bn_pair_cycles: u64,
    /// Cycles spent on KZG evaluation precompile.
    pub kzg_eval_cycles: u64,
    /// Cycles spent on secp256k1 EC recover precompile.
    pub ec_recover_cycles: u64,
    /// Cycles spent on P-256 signature verification precompile.
    pub p256_verify_cycles: u64,
}

impl RequestExecutionStatistics {
    /// Extracts cycle-level statistics from the given execution report.
    pub fn new(execution_report: ExecutionReport) -> Self {
        let get_cycles = |key: &str| *execution_report.cycle_tracker.get(key).unwrap_or(&0);

        Self {
            total_instruction_cycles: execution_report.total_instruction_count(),
            total_sp1_gas: execution_report.gas().unwrap_or(0),
            block_execution_cycles: get_cycles("block-execution"),
            oracle_verify_cycles: get_cycles("oracle-verify"),
            derivation_cycles: get_cycles("payload-derivation"),
            blob_verification_cycles: get_cycles("blob-verification"),
            bn_add_cycles: get_cycles(keys::BN_ADD),
            bn_mul_cycles: get_cycles(keys::BN_MUL),
            bn_pair_cycles: get_cycles(keys::BN_PAIR),
            kzg_eval_cycles: get_cycles(keys::KZG_EVAL),
            ec_recover_cycles: get_cycles(keys::EC_RECOVER),
            p256_verify_cycles: get_cycles(keys::P256_VERIFY),
        }
    }
}

impl From<&ProofRequest> for RequestExecutionStatistics {
    fn from(value: &ProofRequest) -> Self {
        Self {
            total_instruction_cycles: value.cycles(),
            total_sp1_gas: value.gas_used(),
            block_execution_cycles: 0,
            oracle_verify_cycles: 0,
            derivation_cycles: 0,
            blob_verification_cycles: 0,
            bn_add_cycles: 0,
            bn_mul_cycles: 0,
            bn_pair_cycles: 0,
            kzg_eval_cycles: 0,
            ec_recover_cycles: 0,
            p256_verify_cycles: 0,
        }
    }
}
