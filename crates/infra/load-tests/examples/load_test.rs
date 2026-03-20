//! Load test runner that submits transactions at a target gas-per-second rate.

use std::path::PathBuf;

use alloy_signer_local::PrivateKeySigner;
use base_load_tests::{LoadRunner, RpcClient, TestConfig, init_tracing};
use eyre::{Result, bail};

fn resolve_funder_key(config_key: &str) -> Result<String> {
    if let Ok(env_key) = std::env::var("FUNDER_KEY") {
        return Ok(env_key);
    }

    if config_key.starts_with("${") || config_key == "<your key here>" {
        bail!("FUNDER_KEY environment variable required for this config");
    }

    Ok(config_key.to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => match option_env!("CARGO_MANIFEST_DIR") {
            Some(dir) => PathBuf::from(dir).join("examples/devnet.yaml"),
            None => bail!("usage: spam <config.yaml>"),
        },
    };

    if !config_path.exists() {
        bail!("config file not found: {}", config_path.display());
    }

    println!("=== Baseline Transaction Spammer ===");

    let test_config = TestConfig::load(&config_path)?;

    let rpc_url = test_config.rpc.parse()?;
    let client = RpcClient::new(rpc_url);
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

    println!("Funding test accounts...");
    let funder_key = resolve_funder_key(&test_config.funder_key)?;
    let funding_key: PrivateKeySigner = funder_key.parse()?;
    let funding_amount = test_config.parse_funding_amount()?;
    runner.fund_accounts(funding_key, funding_amount).await?;
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
    println!(
        "Gas: total={}  avg/tx={}",
        summary.gas.total_gas, summary.gas.avg_gas
    );

    Ok(())
}
