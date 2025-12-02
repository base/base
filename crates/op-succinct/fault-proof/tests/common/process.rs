//! Process management utilities for running proposer and challenger tasks.
use std::{num::NonZero, sync::Arc};

use alloy_primitives::Address;
use alloy_provider::ProviderBuilder;
use anyhow::Result;
use fault_proof::{
    challenger::OPSuccinctChallenger,
    config::{ChallengerConfig, RangeSplitCount},
    contract::DisputeGameFactory,
    proposer::OPSuccinctProposer,
};
use op_succinct_host_utils::{
    fetcher::{OPSuccinctDataFetcher, RPCConfig},
    host::OPSuccinctHost,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_signer_utils::SignerLock;
use sp1_sdk::{network::FulfillmentStrategy, SP1ProofMode};
use tracing::Instrument;

pub async fn init_proposer(
    rpc_config: &RPCConfig,
    private_key: &str,
    factory_address: &Address,
    game_type: u32,
) -> Result<OPSuccinctProposer<fault_proof::L1Provider, impl OPSuccinctHost + Clone>> {
    // Create signer directly from private key
    let signer = SignerLock::new(op_succinct_signer_utils::Signer::new_local_signer(private_key)?);

    // Create proposer config with test-specific settings
    let config = fault_proof::config::ProposerConfig {
        l1_rpc: rpc_config.l1_rpc.clone(),
        l2_rpc: rpc_config.l2_rpc.clone(),
        factory_address: *factory_address,
        mock_mode: true,
        fast_finality_mode: false,
        range_proof_strategy: FulfillmentStrategy::Hosted,
        agg_proof_strategy: FulfillmentStrategy::Hosted,
        agg_proof_mode: SP1ProofMode::Plonk,
        proposal_interval_in_blocks: 10, // Much smaller interval for testing
        fetch_interval: 5,               // Check more frequently in tests
        game_type,
        max_concurrent_defense_tasks: 0,
        safe_db_fallback: false,
        metrics_port: 9000,
        fast_finality_proving_limit: 0,
        use_kms_requester: false,
        max_price_per_pgu: 300_000_000, // 0.3 PROVE per billion PGU
        min_auction_period: 1,
        timeout: 14400, // 4 hours
        range_cycle_limit: 1_000_000_000_000,
        range_gas_limit: 1_000_000_000_000,
        range_split_count: RangeSplitCount::one(),
        max_concurrent_range_proofs: NonZero::<usize>::MIN,
        agg_cycle_limit: 1_000_000_000_000,
        agg_gas_limit: 1_000_000_000_000,
        whitelist: None,
    };

    let l1_provider = ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone());
    let factory = DisputeGameFactory::new(*factory_address, l1_provider.clone());

    let fetcher = Arc::new(OPSuccinctDataFetcher::new_with_rollup_config().await?);
    let host = initialize_host(fetcher.clone());

    OPSuccinctProposer::new(config, signer, factory, fetcher, host).await
}

/// Start a proposer, and return a handle to the proposer task.
pub async fn start_proposer(
    rpc_config: &RPCConfig,
    private_key: &str,
    factory_address: &Address,
    game_type: u32,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let proposer = init_proposer(rpc_config, private_key, factory_address, game_type).await?;
    Ok(tokio::spawn(async move {
        Arc::new(proposer).run().instrument(tracing::info_span!("PROPOSER")).await
    }))
}

/// Initialize a challenger without starting its run loop.
pub async fn init_challenger(
    rpc_config: &RPCConfig,
    private_key: &str,
    factory_address: &Address,
    game_type: u32,
    malicious_percentage: Option<f64>,
) -> Result<OPSuccinctChallenger<fault_proof::L1Provider>> {
    let signer = SignerLock::new(op_succinct_signer_utils::Signer::new_local_signer(private_key)?);

    let config = ChallengerConfig {
        l1_rpc: rpc_config.l1_rpc.clone(),
        l2_rpc: rpc_config.l2_rpc.clone(),
        factory_address: *factory_address,
        fetch_interval: 2,
        game_type,
        metrics_port: 9001,
        malicious_challenge_percentage: malicious_percentage.unwrap_or(0.0),
    };

    let l1_provider = ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone());
    let factory = DisputeGameFactory::new(*factory_address, l1_provider.clone());

    OPSuccinctChallenger::new(config, l1_provider, factory, signer).await
}

/// Start a challenger, and return a handle to the challenger task.
pub async fn start_challenger(
    rpc_config: &RPCConfig,
    private_key: &str,
    factory_address: &Address,
    game_type: u32,
    malicious_percentage: Option<f64>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    // Initialize challenger with test configuration but do not run yet.
    let challenger =
        init_challenger(rpc_config, private_key, factory_address, game_type, malicious_percentage)
            .await?;

    Ok(tokio::spawn(async move {
        let mut challenger = challenger;
        challenger.run().instrument(tracing::info_span!("CHALLENGER")).await
    }))
}
