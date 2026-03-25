//! Load test runner that submits transactions at a target gas-per-second rate.

use std::path::PathBuf;

use alloy_primitives::utils::format_ether;
use base_load_tests::{LoadRunner, RpcClient, TestConfig, init_tracing};
use eyre::{Result, bail};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => match option_env!("CARGO_MANIFEST_DIR") {
            Some(dir) => PathBuf::from(dir).join("examples/devnet.yaml"),
            None => bail!("usage: load_test <config.yaml>"),
        },
    };

    if !config_path.exists() {
        bail!("config file not found: {}", config_path.display());
    }

    println!("=== Base Load Test Runner ===");

    let test_config = TestConfig::load(&config_path)?;

    let client = RpcClient::new(test_config.rpc.clone());
    let rpc_chain_id =
        if test_config.chain_id.is_none() { Some(client.chain_id().await?) } else { None };

    let load_config = test_config.to_load_config(rpc_chain_id)?;

    println!(
        "Config: {} | RPC: {} | Chain: {}",
        config_path.display(),
        test_config.rpc,
        load_config.chain_id
    );
    println!(
        "Target: {} GPS | Duration: {:?} | Accounts: {}",
        load_config.target_gps, load_config.duration, load_config.account_count
    );
    println!();

    let mut runner = LoadRunner::new(load_config)?;

    let funding_key = TestConfig::funder_key()?;
    let funding_amount = test_config.parse_funding_amount()?;

    println!("Funding test accounts...");
    runner.fund_accounts(funding_key.clone(), funding_amount).await?;
    println!("Accounts funded.");
    println!();

    println!("Running load test...");
    let summary = runner.run().await?;

    println!();
    println!("=== Results ===");
    println!(
        "Submitted: {} | Confirmed: {} | Failed: {}",
        summary.throughput.total_submitted,
        summary.throughput.total_confirmed,
        summary.throughput.total_failed
    );
    println!(
        "TPS: {:.2} | GPS: {:.0} | Success: {:.1}%",
        summary.throughput.tps,
        summary.throughput.gps,
        summary.throughput.success_rate()
    );
    println!();
    println!(
        "Latency: min={:.1?}  p50={:.1?}  mean={:.1?}  p99={:.1?}  max={:.1?}",
        summary.latency.min,
        summary.latency.p50,
        summary.latency.mean,
        summary.latency.p99,
        summary.latency.max
    );
    println!("Gas: total={}  avg/tx={}", summary.gas.total_gas, summary.gas.avg_gas);
    println!();

    println!("Draining accounts back to funder...");
    let drained = runner.drain_accounts(funding_key).await?;
    println!("Drained {} ETH back to funder.", format_ether(drained));

    Ok(())
}
