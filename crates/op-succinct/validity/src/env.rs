use std::env;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use anyhow::Result;
use reqwest::Url;
use sp1_sdk::{network::FulfillmentStrategy, SP1ProofMode};
use std::str::FromStr;

use crate::ProposerSigner;

#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    pub db_url: String,
    pub metrics_port: u16,
    pub l1_rpc: Url,
    pub proposer_signer: ProposerSigner,
    pub prover_address: Address,
    pub loop_interval: u64,
    pub range_proof_strategy: FulfillmentStrategy,
    pub agg_proof_strategy: FulfillmentStrategy,
    pub agg_proof_mode: SP1ProofMode,
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub range_proof_interval: u64,
    pub max_concurrent_witness_gen: u64,
    pub max_concurrent_proof_requests: u64,
    pub submission_interval: u64,
    pub mock: bool,
    pub safe_db_fallback: bool,
}

/// Helper function to get environment variables with a default value and parse them.
fn get_env_var<T>(key: &str, default: Option<T>) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Debug,
{
    match env::var(key) {
        Ok(value) => {
            value.parse::<T>().map_err(|e| anyhow::anyhow!("Failed to parse {}: {:?}", key, e))
        }
        Err(_) => match default {
            Some(default_val) => Ok(default_val),
            None => anyhow::bail!("{} is not set", key),
        },
    }
}

// 1 minute default loop interval.
const DEFAULT_LOOP_INTERVAL: u64 = 60;

/// Read proposer environment variables and return a config.
///
/// Signer address and signer URL take precedence over private key.
pub fn read_proposer_env() -> Result<EnvironmentConfig> {
    let proposer_signer = if let (Some(signer_url), Some(signer_address)) =
        (env::var("SIGNER_URL").ok(), env::var("SIGNER_ADDRESS").ok())
    {
        let signer_url = Url::parse(&signer_url).expect("Failed to parse SIGNER_URL");
        let signer_address =
            Address::from_str(&signer_address).expect("Failed to parse SIGNER_ADDRESS");
        ProposerSigner::Web3Signer(signer_url, signer_address)
    } else if let Ok(private_key) = env::var("PRIVATE_KEY") {
        let private_key =
            PrivateKeySigner::from_str(&private_key).expect("Failed to parse PRIVATE_KEY");
        ProposerSigner::LocalSigner(private_key)
    } else {
        anyhow::bail!("Neither PRIVATE_KEY nor Web3Signer is set");
    };

    // The prover address takes precedence over the signer address. Note: Setting the prover address
    // in the context of the OP Succinct proposer typically does not make sense, as the contract
    // will verify `tx.origin` matches the `proverAddress`.
    let prover_address = get_env_var("PROVER_ADDRESS", Some(proposer_signer.address()))?;

    // Parse strategy values
    let range_proof_strategy = if get_env_var("RANGE_PROOF_STRATEGY", Some("reserved".to_string()))?
        .to_lowercase() ==
        "hosted"
    {
        FulfillmentStrategy::Hosted
    } else {
        FulfillmentStrategy::Reserved
    };

    let agg_proof_strategy = if get_env_var("AGG_PROOF_STRATEGY", Some("reserved".to_string()))?
        .to_lowercase() ==
        "hosted"
    {
        FulfillmentStrategy::Hosted
    } else {
        FulfillmentStrategy::Reserved
    };

    // Parse proof mode
    let agg_proof_mode =
        if get_env_var("AGG_PROOF_MODE", Some("groth16".to_string()))?.to_lowercase() == "plonk" {
            SP1ProofMode::Plonk
        } else {
            SP1ProofMode::Groth16
        };

    // Optional loop interval
    let loop_interval = get_env_var("LOOP_INTERVAL", Some(DEFAULT_LOOP_INTERVAL))?;

    let config = EnvironmentConfig {
        metrics_port: get_env_var("METRICS_PORT", Some(8080))?,
        l1_rpc: get_env_var("L1_RPC", None)?,
        proposer_signer,
        prover_address,
        db_url: get_env_var("DATABASE_URL", None)?,
        range_proof_strategy,
        agg_proof_strategy,
        agg_proof_mode,
        l2oo_address: get_env_var("L2OO_ADDRESS", Some(Address::ZERO))?,
        dgf_address: get_env_var("DGF_ADDRESS", Some(Address::ZERO))?,
        range_proof_interval: get_env_var("RANGE_PROOF_INTERVAL", Some(1800))?,
        max_concurrent_witness_gen: get_env_var("MAX_CONCURRENT_WITNESS_GEN", Some(1))?,
        max_concurrent_proof_requests: get_env_var("MAX_CONCURRENT_PROOF_REQUESTS", Some(1))?,
        submission_interval: get_env_var("SUBMISSION_INTERVAL", Some(1800))?,
        mock: get_env_var("OP_SUCCINCT_MOCK", Some(false))?,
        loop_interval,
        safe_db_fallback: get_env_var("SAFE_DB_FALLBACK", Some(false))?,
    };

    Ok(config)
}
