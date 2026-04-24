use std::{env, str::FromStr};

use alloy_primitives::Address;
use anyhow::Result;
use base_proof_succinct_host_utils::network::parse_fulfillment_strategy;
use base_proof_succinct_signer_utils::SignerLock;
use reqwest::Url;
use sp1_sdk::{SP1ProofMode, network::FulfillmentStrategy};

/// Environment-sourced configuration for the validity proposer.
#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    /// Database connection URL.
    pub db_url: String,
    /// Port for the Prometheus metrics server.
    pub metrics_port: u16,
    /// L1 RPC endpoint URL.
    pub l1_rpc: Url,
    /// Transaction signer.
    pub signer: SignerLock,
    /// Interval in seconds between proposer loop iterations.
    pub loop_interval: u64,
    /// Fulfillment strategy for range proofs.
    pub range_proof_strategy: FulfillmentStrategy,
    /// Fulfillment strategy for aggregation proofs.
    pub agg_proof_strategy: FulfillmentStrategy,
    /// SP1 proof mode for aggregation proofs.
    pub agg_proof_mode: SP1ProofMode,
    /// Address of the L2 Output Oracle contract.
    pub l2oo_address: Address,
    /// Address of the Dispute Game Factory contract.
    pub dgf_address: Address,
    /// EVM gas limit for range proofs. Zero disables gas-based splitting.
    pub evm_gas_limit: u64,
    /// Number of blocks per range proof when gas-based splitting is disabled.
    pub range_proof_interval: u64,
    /// Maximum number of concurrent witness generation tasks.
    pub max_concurrent_witness_gen: u64,
    /// Maximum number of concurrent proof requests.
    pub max_concurrent_proof_requests: u64,
    /// Minimum number of L2 blocks between on-chain submissions.
    pub submission_interval: u64,
    /// Whether to generate mock proofs instead of real proofs.
    pub mock: bool,
    /// Whether to fall back to timestamp-based L1 head estimation.
    pub safe_db_fallback: bool,
    /// Name of the succinct configuration.
    pub base_proof_succinct_config_name: String,
    /// Whether to use AWS KMS for the network requester key.
    pub use_kms_requester: bool,
    /// Maximum price per PGU for proving.
    pub max_price_per_pgu: u64,
    /// Timeout in seconds for proving.
    pub proving_timeout: u64,
    /// Timeout in seconds for network prover calls.
    pub network_calls_timeout: u64,
    /// Cycle limit for range proofs.
    pub range_cycle_limit: u64,
    /// Gas limit for range proofs.
    pub range_gas_limit: u64,
    /// Cycle limit for aggregation proofs.
    pub agg_cycle_limit: u64,
    /// Gas limit for aggregation proofs.
    pub agg_gas_limit: u64,
    /// Allowlist of prover addresses permitted to bid on proof requests.
    pub whitelist: Option<Vec<Address>>,
    /// Minimum auction period in seconds.
    pub min_auction_period: u64,
    /// Timeout in seconds before cancelling an unassigned proof request.
    pub auction_timeout: u64,
}

/// Helper function to get environment variables with a default value and parse them.
fn get_env_var<T>(key: &str, default: Option<T>) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Debug,
{
    match env::var(key) {
        Ok(value) => {
            value.parse::<T>().map_err(|e| anyhow::anyhow!("Failed to parse {key}: {e:?}"))
        }
        Err(_) => match default {
            Some(default_val) => Ok(default_val),
            None => anyhow::bail!("{key} is not set"),
        },
    }
}

/// Helper function to parse a comma-separated list of addresses
fn parse_whitelist(whitelist_str: &str) -> Result<Option<Vec<Address>>> {
    if whitelist_str.is_empty() {
        return Ok(None);
    }

    let addresses: Result<Vec<Address>> = whitelist_str
        .split(',')
        .map(|addr_str| {
            let addr_str = addr_str.trim().trim_start_matches("0x");
            // Add 0x prefix since addresses are provided without it
            let addr_with_prefix = format!("0x{addr_str}");
            Address::from_str(&addr_with_prefix)
                .map_err(|e| anyhow::anyhow!("Failed to parse address '{addr_str}': {e:?}"))
        })
        .collect();

    addresses.map(|addrs| if addrs.is_empty() { None } else { Some(addrs) })
}

// 1 minute default loop interval.
const DEFAULT_LOOP_INTERVAL: u64 = 60;

/// Read proposer environment variables and return a config.
///
/// Signer address and signer URL take precedence over private key.
pub async fn read_proposer_env() -> Result<EnvironmentConfig> {
    let signer = SignerLock::from_env().await?;

    // Parse strategy values
    let range_proof_strategy = parse_fulfillment_strategy(get_env_var(
        "RANGE_PROOF_STRATEGY",
        Some("reserved".to_string()),
    )?)?;
    let agg_proof_strategy = parse_fulfillment_strategy(get_env_var(
        "AGG_PROOF_STRATEGY",
        Some("reserved".to_string()),
    )?)?;

    // Parse proof mode
    let agg_proof_mode =
        if get_env_var("AGG_PROOF_MODE", Some("plonk".to_string()))?.to_lowercase() == "groth16" {
            SP1ProofMode::Groth16
        } else {
            SP1ProofMode::Plonk
        };

    // Optional loop interval
    let loop_interval = get_env_var("LOOP_INTERVAL", Some(DEFAULT_LOOP_INTERVAL))?;

    let config = EnvironmentConfig {
        metrics_port: get_env_var("METRICS_PORT", Some(8080))?,
        l1_rpc: get_env_var("L1_RPC", None)?,
        signer,
        db_url: get_env_var("DATABASE_URL", None)?,
        range_proof_strategy,
        agg_proof_strategy,
        agg_proof_mode,
        l2oo_address: get_env_var("L2OO_ADDRESS", Some(Address::ZERO))?,
        dgf_address: get_env_var("DGF_ADDRESS", Some(Address::ZERO))?,
        evm_gas_limit: get_env_var("RANGE_PROOF_EVM_GAS_LIMIT", Some(0))?,
        range_proof_interval: get_env_var("RANGE_PROOF_INTERVAL", Some(1800))?,
        max_concurrent_witness_gen: get_env_var("MAX_CONCURRENT_WITNESS_GEN", Some(1))?,
        max_concurrent_proof_requests: get_env_var("MAX_CONCURRENT_PROOF_REQUESTS", Some(1))?,
        submission_interval: get_env_var("SUBMISSION_INTERVAL", Some(1800))?,
        mock: get_env_var("OP_SUCCINCT_MOCK", Some(false))?,
        loop_interval,
        safe_db_fallback: get_env_var("SAFE_DB_FALLBACK", Some(false))?,
        base_proof_succinct_config_name: get_env_var(
            "OP_SUCCINCT_CONFIG_NAME",
            Some("opsuccinct_genesis".to_string()),
        )?,
        use_kms_requester: get_env_var("USE_KMS_REQUESTER", Some(false))?,
        max_price_per_pgu: get_env_var("MAX_PRICE_PER_PGU", Some(300_000_000))?, /* 0.3 PROVE per billion PGU */
        proving_timeout: get_env_var("PROVING_TIMEOUT", Some(14400))?,           // 4 hours
        network_calls_timeout: get_env_var("NETWORK_CALLS_TIMEOUT", Some(15))?,  // 15 seconds
        range_cycle_limit: get_env_var("RANGE_CYCLE_LIMIT", Some(1_000_000_000_000))?, // 1 trillion
        range_gas_limit: get_env_var("RANGE_GAS_LIMIT", Some(1_000_000_000_000))?, // 1 trillion
        agg_cycle_limit: get_env_var("AGG_CYCLE_LIMIT", Some(1_000_000_000_000))?, // 1 trillion
        agg_gas_limit: get_env_var("AGG_GAS_LIMIT", Some(1_000_000_000_000))?,   // 1 trillion
        whitelist: parse_whitelist(&get_env_var("WHITELIST", Some(String::new()))?)?,
        min_auction_period: get_env_var("MIN_AUCTION_PERIOD", Some(1))?,
        auction_timeout: get_env_var("AUCTION_TIMEOUT", Some(60))?, // 1 minute
    };

    Ok(config)
}
