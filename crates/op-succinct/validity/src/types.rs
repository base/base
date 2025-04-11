use alloy_primitives::B256;
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Deserializer, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use sp1_sdk::{
    network::FulfillmentStrategy, ExecutionReport, NetworkProver, SP1ProofMode, SP1ProvingKey,
    SP1VerifyingKey,
};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug)]
pub struct ValidateConfigRequest {
    pub address: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ValidateConfigResponse {
    pub rollup_config_hash_valid: bool,
    pub agg_vkey_valid: bool,
    pub range_vkey_valid: bool,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SpanProofRequest {
    pub start: u64,
    pub end: u64,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct AggProofRequest {
    #[serde(deserialize_with = "deserialize_base64_vec")]
    pub subproofs: Vec<Vec<u8>>,
    pub head: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct MockProofResponse {
    pub proof_id: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProofResponse {
    pub proof_id: Vec<u8>,
}

#[derive(Debug, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
/// The type of error that occurred when unclaiming a proof. Based off of the `unclaim_description`
/// field in the `ProofStatus` struct.
pub enum UnclaimDescription {
    UnexpectedProverError = 0,
    ProgramExecutionError = 1,
    CycleLimitExceeded = 2,
    Other = 3,
}

/// Convert a string to an `UnclaimDescription`. These cover the common reasons why a proof might
/// be unclaimed.
impl From<String> for UnclaimDescription {
    fn from(description: String) -> Self {
        match description.as_str().to_lowercase().as_str() {
            "unexpected prover error" => UnclaimDescription::UnexpectedProverError,
            "program execution error" => UnclaimDescription::ProgramExecutionError,
            "cycle limit exceeded" => UnclaimDescription::CycleLimitExceeded,
            _ => UnclaimDescription::Other,
        }
    }
}

#[derive(Serialize, Deserialize)]
/// The status of a proof request.
pub struct ProofStatus {
    // Note: Can't use `FulfillmentStatus`/`ExecutionStatus` directly because `Serialize_repr` and
    // `Deserialize_repr` aren't derived on it.
    pub fulfillment_status: i32,
    pub execution_status: i32,
    pub proof: Vec<u8>,
}

/// Configuration of the L2 Output Oracle contract. Created once at server start-up, monitors if
/// there are any changes to the contract's configuration.
#[derive(Clone)]
pub struct SuccinctProposerConfig {
    pub range_vk: Arc<SP1VerifyingKey>,
    pub range_pk: Arc<SP1ProvingKey>,
    pub agg_pk: Arc<SP1ProvingKey>,
    pub agg_vk: Arc<SP1VerifyingKey>,
    pub agg_vkey_hash: B256,
    pub range_vkey_commitment: B256,
    pub rollup_config_hash: B256,
    pub range_proof_strategy: FulfillmentStrategy,
    pub agg_proof_strategy: FulfillmentStrategy,
    pub agg_proof_mode: SP1ProofMode,
    pub network_prover: Arc<NetworkProver>,
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

#[derive(Serialize)]
pub struct RequestExecutionStatistics {
    pub total_instruction_cycles: u64,
    pub total_sp1_gas: u64,
    pub block_execution_cycles: u64,
    pub oracle_verify_cycles: u64,
    pub derivation_cycles: u64,
    pub blob_verification_cycles: u64,
    pub bn_add_cycles: u64,
    pub bn_mul_cycles: u64,
    pub bn_pair_cycles: u64,
    pub kzg_eval_cycles: u64,
    pub ec_recover_cycles: u64,
    pub p256_verify_cycles: u64,
}

impl RequestExecutionStatistics {
    pub fn new(execution_report: ExecutionReport) -> Self {
        let get_cycles = |key: &str| *execution_report.cycle_tracker.get(key).unwrap_or(&0);

        Self {
            total_instruction_cycles: execution_report.total_instruction_count(),
            total_sp1_gas: execution_report.gas.unwrap_or(0),
            block_execution_cycles: get_cycles("block-execution"),
            oracle_verify_cycles: get_cycles("oracle-verify"),
            derivation_cycles: get_cycles("payload-derivation"),
            blob_verification_cycles: get_cycles("blob-verification"),
            bn_add_cycles: get_cycles("precompile-bn-add"),
            bn_mul_cycles: get_cycles("precompile-bn-mul"),
            bn_pair_cycles: get_cycles("precompile-bn-pair"),
            kzg_eval_cycles: get_cycles("precompile-kzg-eval"),
            ec_recover_cycles: get_cycles("precompile-ec-recover"),
            p256_verify_cycles: get_cycles("precompile-p256-verify"),
        }
    }
}
