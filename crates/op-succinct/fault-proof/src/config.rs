use std::{env, str::FromStr};

use alloy_primitives::Address;
use alloy_transport_http::reqwest::Url;
use anyhow::Result;
use op_succinct_host_utils::network::parse_fulfillment_strategy;
use serde::{Deserialize, Serialize};
use sp1_sdk::network::FulfillmentStrategy;

#[derive(Debug, Clone)]
pub struct ProposerConfig {
    /// The L1 RPC URL.
    pub l1_rpc: Url,

    /// The L2 RPC URL.
    pub l2_rpc: Url,

    /// The address of the factory contract.
    pub factory_address: Address,

    /// Whether to use mock mode.
    pub mock_mode: bool,

    /// Whether to use fast finality mode.
    pub fast_finality_mode: bool,

    /// Proof fulfillment strategy for range proofs.
    pub range_proof_strategy: FulfillmentStrategy,

    /// Proof fulfillment strategy for aggregation proofs.
    pub agg_proof_strategy: FulfillmentStrategy,

    /// The interval in blocks between proposing new games.
    pub proposal_interval_in_blocks: u64,

    /// The interval in seconds between checking for new proposals and game resolution.
    /// During each interval, the proposer:
    /// 1. Checks the safe L2 head block number
    /// 2. Gets the latest valid proposal
    /// 3. Creates a new game if conditions are met
    /// 4. Optionally attempts to resolve unchallenged games
    pub fetch_interval: u64,

    /// The type of game to propose.
    pub game_type: u32,

    /// The max number of defense tasks to run concurrently.
    pub max_concurrent_defense_tasks: u64,

    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not
    /// activated for op-node.
    pub safe_db_fallback: bool,

    /// The metrics port.
    pub metrics_port: u16,

    /// Maximum concurrent proving tasks allowed in fast finality mode.
    /// This limit prevents game creation when proving capacity is reached.
    pub fast_finality_proving_limit: u64,

    /// Whether to expect NETWORK_PRIVATE_KEY to be an AWS KMS key ARN instead of a
    /// plaintext private key.
    pub use_kms_requester: bool,

    /// The maximum price per pgu for proving.
    pub max_price_per_pgu: u64,

    /// The minimum auction period (in seconds).
    pub min_auction_period: u64,

    /// The timeout to use for proving (in seconds).
    pub timeout: u64,

    /// The cycle limit to use for range proofs.
    pub range_cycle_limit: u64,

    /// The gas limit to use for range proofs.
    pub range_gas_limit: u64,

    /// The cycle limit to use for aggregation proofs.
    pub agg_cycle_limit: u64,

    /// The gas limit to use for aggregation proofs.
    pub agg_gas_limit: u64,

    /// The list of prover addresses that are allowed to bid on proof requests.
    pub whitelist: Option<Vec<Address>>,
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
            let addr_with_prefix = format!("0x{}", addr_str);
            Address::from_str(&addr_with_prefix)
                .map_err(|e| anyhow::anyhow!("Failed to parse address '{}': {:?}", addr_str, e))
        })
        .collect();

    addresses.map(|addrs| if addrs.is_empty() { None } else { Some(addrs) })
}

impl ProposerConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            l1_rpc: env::var("L1_RPC")?.parse().expect("L1_RPC not set"),
            l2_rpc: env::var("L2_RPC")?.parse().expect("L2_RPC not set"),
            factory_address: env::var("FACTORY_ADDRESS")?.parse().expect("FACTORY_ADDRESS not set"),
            mock_mode: env::var("MOCK_MODE").unwrap_or("false".to_string()).parse()?,
            fast_finality_mode: env::var("FAST_FINALITY_MODE")
                .unwrap_or("false".to_string())
                .parse()?,
            range_proof_strategy: parse_fulfillment_strategy(
                env::var("RANGE_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            agg_proof_strategy: parse_fulfillment_strategy(
                env::var("AGG_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            proposal_interval_in_blocks: env::var("PROPOSAL_INTERVAL_IN_BLOCKS")
                .unwrap_or("1800".to_string())
                .parse()?,
            fetch_interval: env::var("FETCH_INTERVAL").unwrap_or("30".to_string()).parse()?,
            game_type: env::var("GAME_TYPE").expect("GAME_TYPE not set").parse()?,
            max_concurrent_defense_tasks: env::var("MAX_CONCURRENT_DEFENSE_TASKS")
                .unwrap_or("8".to_string())
                .parse()?,
            safe_db_fallback: env::var("SAFE_DB_FALLBACK")
                .unwrap_or("false".to_string())
                .parse()?,
            metrics_port: env::var("PROPOSER_METRICS_PORT")
                .unwrap_or("9000".to_string())
                .parse()?,
            fast_finality_proving_limit: env::var("FAST_FINALITY_PROVING_LIMIT")
                .unwrap_or("1".to_string())
                .parse()?,
            use_kms_requester: env::var("USE_KMS_REQUESTER")
                .unwrap_or("false".to_string())
                .parse()?,
            max_price_per_pgu: env::var("MAX_PRICE_PER_PGU")
                .unwrap_or("300000000".to_string()) // 0.3 PROVE per billion PGU
                .parse()?,
            min_auction_period: env::var("MIN_AUCTION_PERIOD")
                .unwrap_or("1".to_string())
                .parse()?,
            timeout: env::var("TIMEOUT").unwrap_or("14400".to_string()).parse()?, // 4 hours
            range_cycle_limit: env::var("RANGE_CYCLE_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            range_gas_limit: env::var("RANGE_GAS_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            agg_cycle_limit: env::var("AGG_CYCLE_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            agg_gas_limit: env::var("AGG_GAS_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            whitelist: parse_whitelist(&env::var("WHITELIST").unwrap_or("".to_string()))?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChallengerConfig {
    pub l1_rpc: Url,
    pub l2_rpc: Url,
    pub factory_address: Address,

    /// The interval in seconds between checking for new challenges opportunities.
    pub fetch_interval: u64,

    /// The game type to challenge.
    pub game_type: u32,

    /// The metrics port.
    pub metrics_port: u16,

    /// Percentage (0.0-100.0) of valid games to challenge maliciously for testing.
    /// Set to 0.0 (default) for production use (honest challenging only).
    /// Set to >0.0 for testing defense mechanisms.
    pub malicious_challenge_percentage: f64,
}

impl ChallengerConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            l1_rpc: env::var("L1_RPC")?.parse().expect("L1_RPC not set"),
            l2_rpc: env::var("L2_RPC")?.parse().expect("L2_RPC not set"),
            factory_address: env::var("FACTORY_ADDRESS")?.parse().expect("FACTORY_ADDRESS not set"),
            game_type: env::var("GAME_TYPE").expect("GAME_TYPE not set").parse()?,
            fetch_interval: env::var("FETCH_INTERVAL").unwrap_or("30".to_string()).parse()?,
            metrics_port: env::var("CHALLENGER_METRICS_PORT")
                .unwrap_or("9001".to_string())
                .parse()?,
            malicious_challenge_percentage: env::var("MALICIOUS_CHALLENGE_PERCENTAGE")
                .unwrap_or("0.0".to_string())
                .parse()?,
        })
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
/// The config for deploying the OPSuccinctFaultDisputeGame.
/// Note: The fields should be in alphabetical order for Solidity to parse it correctly.
pub struct FaultDisputeGameConfig {
    pub aggregation_vkey: String,
    pub challenger_addresses: Vec<String>,
    pub challenger_bond_wei: u64,
    pub dispute_game_finality_delay_seconds: u64,
    pub fallback_timeout_fp_secs: u64,
    pub game_type: u32,
    pub initial_bond_wei: u64,
    pub max_challenge_duration: u64,
    pub max_prove_duration: u64,
    pub optimism_portal2_address: String,
    pub permissionless_mode: bool,
    pub proposer_addresses: Vec<String>,
    pub range_vkey_commitment: String,
    pub rollup_config_hash: String,
    pub starting_l2_block_number: u64,
    pub starting_root: String,
    pub use_sp1_mock_verifier: bool,
    pub verifier_address: String,
}
