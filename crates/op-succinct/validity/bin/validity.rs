use alloy_provider::{Provider, ProviderBuilder};
use anyhow::Result;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    metrics::{init_metrics, MetricsGauge},
    setup_logger,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_validity::{
    read_proposer_env, DriverDBClient, Proposer, RequesterConfig, ValidityGauge,
};
use std::sync::Arc;
use tikv_jemallocator::Jemalloc;
use tracing::info;

#[global_allocator]
static ALLOCATOR: Jemalloc = Jemalloc;

use clap::Parser;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to environment file
    #[arg(long, default_value = ".env")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let provider = rustls::crypto::ring::default_provider();
    provider
        .install_default()
        .map_err(|e| anyhow::anyhow!("Failed to install default provider: {:?}", e))?;

    let args = Args::parse();

    dotenv::from_filename(args.env_file).ok();

    setup_logger();

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    // Read the environment variables.
    let env_config = read_proposer_env().await?;

    let db_client = Arc::new(DriverDBClient::new(&env_config.db_url).await?);

    let op_succinct_config_name_hash =
        alloy_primitives::keccak256(env_config.op_succinct_config_name.as_bytes());

    // Validate that at least one of gas_limit or range_proof_interval is nonzero
    if env_config.evm_gas_limit == 0 && env_config.range_proof_interval == 0 {
        return Err(anyhow::anyhow!(
            "At least one of GAS_LIMIT or RANGE_PROOF_INTERVAL must be non-zero"
        ));
    }

    let proposer_config = RequesterConfig {
        l1_chain_id: fetcher.l1_provider.get_chain_id().await? as i64,
        l2_chain_id: fetcher.l2_provider.get_chain_id().await? as i64,
        l2oo_address: env_config.l2oo_address,
        dgf_address: env_config.dgf_address,
        evm_gas_limit: env_config.evm_gas_limit,
        range_proof_interval: env_config.range_proof_interval,
        max_concurrent_witness_gen: env_config.max_concurrent_witness_gen,
        max_concurrent_proof_requests: env_config.max_concurrent_proof_requests,
        range_proof_strategy: env_config.range_proof_strategy,
        agg_proof_strategy: env_config.agg_proof_strategy,
        agg_proof_mode: env_config.agg_proof_mode,
        submission_interval: env_config.submission_interval,
        mock: env_config.mock,
        safe_db_fallback: env_config.safe_db_fallback,
        op_succinct_config_name_hash,
        use_kms_requester: env_config.use_kms_requester,
        max_price_per_pgu: env_config.max_price_per_pgu,
        proving_timeout: env_config.proving_timeout,
        network_calls_timeout: env_config.network_calls_timeout,
        range_cycle_limit: env_config.range_cycle_limit,
        range_gas_limit: env_config.range_gas_limit,
        agg_cycle_limit: env_config.agg_cycle_limit,
        agg_gas_limit: env_config.agg_gas_limit,
        whitelist: env_config.whitelist,
        min_auction_period: env_config.min_auction_period,
        auction_timeout: env_config.auction_timeout,
    };

    let l1_provider = ProviderBuilder::new().connect_http(env_config.l1_rpc.clone());

    let host = initialize_host(fetcher.clone().into());

    let proposer = Proposer::new(
        l1_provider,
        db_client.clone(),
        fetcher.into(),
        proposer_config,
        env_config.signer,
        env_config.loop_interval,
        host,
    )
    .await?;

    // Spawn a thread for the proposer.
    info!("Starting proposer.");
    let proposer_handle = tokio::spawn(async move {
        if let Err(e) = proposer.run().await {
            tracing::error!("Proposer error: {}", e);
            return Err(e);
        }
        Ok(())
    });

    // Initialize metrics exporter.
    info!("Initializing metrics on port {}", env_config.metrics_port);
    ValidityGauge::register_all();
    init_metrics(&env_config.metrics_port);

    // Wait for all tasks to complete.
    let proposer_res = proposer_handle.await?;
    if let Err(e) = proposer_res {
        tracing::error!("Proposer task failed: {}", e);
        return Err(e);
    }

    Ok(())
}
