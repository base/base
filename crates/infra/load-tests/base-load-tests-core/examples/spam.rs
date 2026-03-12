#![allow(missing_docs)]

use std::path::PathBuf;

use alloy_signer_local::PrivateKeySigner;
use base_load_tests_core::{LoadRunner, RpcClient, TestConfig, init_tracing};
use eyre::{Result, bail};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/config.yaml")
    });

    if !config_path.exists() {
        bail!("config file not found: {}", config_path.display());
    }

    println!("=== Baseline Transaction Spammer ===");
    println!("Loading config from: {}", config_path.display());

    let test_config = TestConfig::load(&config_path)?;

    let rpc_url = test_config.rpc.parse()?;
    let client = RpcClient::new(rpc_url);
    let rpc_chain_id = if test_config.chain_id.is_none() {
        Some(client.chain_id().await?)
    } else {
        None
    };

    let load_config = test_config.to_load_config(rpc_chain_id)?;

    println!("RPC URL: {}", test_config.rpc);
    println!("Chain ID: {}", load_config.chain_id);
    println!("Target TPS: {}", load_config.tps);
    println!("Duration: {:?}", load_config.duration);
    println!("Accounts: {}", load_config.account_count);
    println!("Max in-flight per sender: {}", load_config.max_in_flight_per_sender);
    println!("Transaction types: {}", test_config.transactions.len());
    println!();

    let mut runner = LoadRunner::new(load_config)?;

    println!("Funding test accounts...");
    let funding_key: PrivateKeySigner = test_config.funder_key.parse()?;
    let funding_amount = test_config.parse_funding_amount()?;
    runner.fund_accounts(funding_key, funding_amount).await?;
    println!("Accounts funded!");
    println!();

    println!("Starting load test...");
    let summary = runner.run().await?;

    println!();
    println!("=== Results ===");
    println!("Submitted: {}", summary.throughput.total_submitted);
    println!("Confirmed: {}", summary.throughput.total_confirmed);
    println!("Failed: {}", summary.throughput.total_failed);
    println!("TPS: {:.2}", summary.throughput.tps);
    println!("Success Rate: {:.2}%", summary.throughput.success_rate());
    println!();
    println!("Latency:");
    println!("  Min: {:?}", summary.latency.min);
    println!("  Max: {:?}", summary.latency.max);
    println!("  Mean: {:?}", summary.latency.mean);
    println!("  P50: {:?}", summary.latency.p50);
    println!("  P99: {:?}", summary.latency.p99);
    println!();
    println!("Gas:");
    println!("  Total: {}", summary.gas.total_gas);
    println!("  Avg per tx: {}", summary.gas.avg_gas);

    Ok(())
}
