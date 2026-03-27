//! Load test runner binary that submits transactions at a target gas-per-second rate.

use std::path::PathBuf;

use alloy_primitives::utils::format_ether;
use base_load_tests::{LoadRunner, LoadTestDisplay, RpcClient, TestConfig};
use eyre::{Result, bail};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mp = LoadTestDisplay::init_tracing();

    let mut args = std::env::args().skip(1).peekable();
    let mut config_path: Option<PathBuf> = None;
    let mut continuous = false;

    for arg in args.by_ref() {
        match arg.as_str() {
            "--continuous" => continuous = true,
            other => {
                if config_path.is_none() {
                    config_path = Some(PathBuf::from(other));
                }
            }
        }
    }

    let config_path = config_path
        .or_else(|| {
            option_env!("CARGO_MANIFEST_DIR")
                .map(|dir| PathBuf::from(dir).join("examples/devnet.yaml"))
        })
        .ok_or_else(|| eyre::eyre!("usage: base-load-test [--continuous] <config.yaml>"))?;

    if !config_path.exists() {
        bail!("config file not found: {}", config_path.display());
    }

    println!("=== Base Load Test Runner ===");

    let test_config = TestConfig::load(&config_path)?;

    let client = RpcClient::new(test_config.rpc.clone());
    let rpc_chain_id =
        if test_config.chain_id.is_none() { Some(client.chain_id().await?) } else { None };

    let load_config = {
        let cfg = test_config.to_load_config(rpc_chain_id)?;
        if continuous { cfg.with_continuous() } else { cfg }
    };

    println!(
        "Config: {} | RPC: {} | Chain: {}",
        config_path.display(),
        test_config.rpc,
        load_config.chain_id
    );
    let duration_display =
        load_config.duration.map_or_else(|| "continuous".to_string(), |d| format!("{d:?}"));
    println!(
        "Target: {} GPS | Duration: {} | Accounts: {}",
        load_config.target_gps, duration_display, load_config.account_count
    );
    println!();

    let funding_key = TestConfig::funder_key()?;
    let funding_amount = test_config.parse_funding_amount()?;

    let mut runner = LoadRunner::new(load_config.clone())?;

    println!("Funding test accounts...");
    runner.fund_accounts(funding_key.clone(), funding_amount).await?;
    println!("Accounts funded.");
    println!();

    println!("Running load test...");

    // Create bars after all pre-run println output so setup text doesn't
    // interleave with the live display.
    let display = LoadTestDisplay::new(&mp, load_config.duration);
    runner.set_display(display);

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
