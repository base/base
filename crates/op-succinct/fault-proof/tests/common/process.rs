//! Process management utilities for running proposer and challenger tasks.
use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::ProviderBuilder;
use anyhow::Result;
use fault_proof::{
    challenger::OPSuccinctChallenger,
    config::ChallengerConfig,
    contract::{AnchorStateRegistry, DisputeGameFactory, OPSuccinctFaultDisputeGame},
    proposer::OPSuccinctProposer,
};
use op_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RPCConfig};
use op_succinct_proof_utils::initialize_host;
use op_succinct_signer_utils::Signer;
use sp1_sdk::network::FulfillmentStrategy;
use tracing::Instrument;

/// Start a proposer, and return a handle to the proposer task.
pub async fn start_proposer(
    rpc_config: &RPCConfig,
    private_key: &str,
    factory_address: &Address,
    game_type: u32,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    // Create signer directly from private key
    let signer = Signer::new_local_signer(private_key)?;

    // Create proposer config with test-specific settings
    let config = fault_proof::config::ProposerConfig {
        l1_rpc: rpc_config.l1_rpc.clone(),
        l2_rpc: rpc_config.l2_rpc.clone(),
        factory_address: *factory_address,
        mock_mode: true,
        fast_finality_mode: false,
        range_proof_strategy: FulfillmentStrategy::Hosted,
        agg_proof_strategy: FulfillmentStrategy::Hosted,
        proposal_interval_in_blocks: 10, // Much smaller interval for testing
        fetch_interval: 2,               // Check more frequently in tests
        game_type,
        max_concurrent_defense_tasks: 1,
        safe_db_fallback: false,
        metrics_port: 9000,
        fast_finality_proving_limit: 1,
        use_kms_requester: false,
        max_price_per_pgu: 300_000_000, // 0.3 PROVE per billion PGU
        min_auction_period: 1,
        timeout: 14400, // 4 hours
        range_cycle_limit: 1_000_000_000_000,
        range_gas_limit: 1_000_000_000_000,
        agg_cycle_limit: 1_000_000_000_000,
        agg_gas_limit: 1_000_000_000_000,
        whitelist: None,
    };

    let l1_provider = ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone());
    let factory = DisputeGameFactory::new(*factory_address, l1_provider.clone());

    let game_impl_address = factory.gameImpls(config.game_type).call().await?;
    let game_impl = OPSuccinctFaultDisputeGame::new(game_impl_address, l1_provider.clone());
    let anchor_state_registry_address = game_impl.anchorStateRegistry().call().await?;
    let anchor_state_registry =
        AnchorStateRegistry::new(anchor_state_registry_address, l1_provider.clone());

    let fetcher = Arc::new(OPSuccinctDataFetcher::new_with_rollup_config().await?);
    let host = initialize_host(fetcher.clone());

    Ok(tokio::spawn(async move {
        let proposer =
            OPSuccinctProposer::new(config, signer, factory, anchor_state_registry, fetcher, host)
                .await?;
        Arc::new(proposer).run().instrument(tracing::info_span!("PROPOSER")).await
    }))
}

/// Start a challenger, and return a handle to the challenger task.
pub async fn start_challenger(
    rpc_config: &RPCConfig,
    private_key: &str,
    factory_address: &Address,
    game_type: u32,
    malicious_percentage: Option<f64>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    // Create signer directly from private key
    let signer = Signer::new_local_signer(private_key)?;

    // Create challenger config with test-specific settings
    let config = ChallengerConfig {
        l1_rpc: rpc_config.l1_rpc.clone(),
        l2_rpc: rpc_config.l2_rpc.clone(),
        factory_address: *factory_address,
        fetch_interval: 2, // Check more frequently in tests
        game_type,
        max_games_to_check_for_challenge: 10, // Check more games
        enable_game_resolution: true,
        max_games_to_check_for_resolution: 100,
        max_games_to_check_for_bond_claiming: 100,
        metrics_port: 9001,
        malicious_challenge_percentage: malicious_percentage.unwrap_or(0.0),
    };

    let l1_provider = ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone());
    let factory = DisputeGameFactory::new(*factory_address, l1_provider.clone());

    Ok(tokio::spawn(async move {
        let mut challenger =
            OPSuccinctChallenger::new_with_config(config, l1_provider.clone(), factory, signer)
                .await?;
        challenger.run().instrument(tracing::info_span!("CHALLENGER")).await
    }))
}
