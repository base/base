//! Load test runner binary that submits transactions at a target gas-per-second rate.
//!
//! Also provides a `rescue` subcommand for recovering stranded funds from
//! test accounts after failed or interrupted load test runs.

use std::{path::PathBuf, time::Duration};

use alloy_network::{EthereumWallet, ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, TxHash, U256, utils::format_ether};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use base_load_tests::{
    AccountPool, BaselineError, FundedAccount, LoadRunner, LoadTestDisplay, Result as LoadResult,
    RpcClient, TestConfig, create_wallet_provider,
};
use eyre::{Result, bail};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, info, warn};

/// Accounts to derive and check per batch during rescue.
const RESCUE_BATCH_SIZE: usize = 100;

/// Maximum concurrent RPC requests during rescue.
const RESCUE_CONCURRENCY: usize = 32;

/// Default number of accounts to scan during rescue.
const DEFAULT_RESCUE_SCAN_COUNT: usize = 1000;

/// Default maximum gas price (1000 gwei).
const DEFAULT_MAX_GAS_PRICE: u128 = 1_000_000_000_000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).peekable();

    match args.peek().map(String::as_str) {
        Some("rescue") => {
            args.next();
            run_rescue(args.collect()).await
        }
        _ => run_load_test(args.collect()).await,
    }
}

// ---------------------------------------------------------------------------
// load-test subcommand (default)
// ---------------------------------------------------------------------------

async fn run_load_test(args: Vec<String>) -> Result<()> {
    let mp = LoadTestDisplay::init_tracing();

    let mut config_path: Option<PathBuf> = None;
    let mut continuous = false;

    for arg in &args {
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
    let fb = &summary.flashblocks_latency;
    println!(
        "Flashblocks Latency: p50={:.1?}  p90={:.1?}  p99={:.1?}  (n={})",
        fb.p50, fb.p90, fb.p99, fb.count
    );
    let bl = &summary.block_latency;
    println!(
        "Block Latency: min={:.1?}  p50={:.1?}  mean={:.1?}  p99={:.1?}  max={:.1?}",
        bl.min, bl.p50, bl.mean, bl.p99, bl.max
    );
    println!("Gas: total={}  avg/tx={}", summary.gas.total_gas, summary.gas.avg_gas);
    println!();

    println!("Draining accounts back to funder...");
    let drained = runner.drain_accounts(funding_key).await?;
    println!("Drained {} ETH back to funder.", format_ether(drained));

    Ok(())
}

// ---------------------------------------------------------------------------
// rescue subcommand
// ---------------------------------------------------------------------------

struct RescueArgs {
    rpc_url: url::Url,
    seed: u64,
    scan_count: usize,
    offset: usize,
    funder_key: PrivateKeySigner,
    mnemonic: Option<String>,
}

impl RescueArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut rpc_url: Option<url::Url> = None;
        let mut seed: Option<u64> = None;
        let mut scan_count = DEFAULT_RESCUE_SCAN_COUNT;
        let mut offset = 0usize;
        let mut mnemonic: Option<String> = None;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--rpc-url" | "--rpc" => {
                    i += 1;
                    rpc_url = Some(
                        args.get(i)
                            .ok_or_else(|| eyre::eyre!("--rpc-url requires a value"))?
                            .parse()?,
                    );
                }
                "--seed" => {
                    i += 1;
                    seed = Some(
                        args.get(i)
                            .ok_or_else(|| eyre::eyre!("--seed requires a value"))?
                            .parse()?,
                    );
                }
                "--count" => {
                    i += 1;
                    scan_count = args
                        .get(i)
                        .ok_or_else(|| eyre::eyre!("--count requires a value"))?
                        .parse()?;
                }
                "--offset" => {
                    i += 1;
                    offset = args
                        .get(i)
                        .ok_or_else(|| eyre::eyre!("--offset requires a value"))?
                        .parse()?;
                }
                "--mnemonic" => {
                    i += 1;
                    mnemonic = Some(
                        args.get(i)
                            .ok_or_else(|| eyre::eyre!("--mnemonic requires a value"))?
                            .clone(),
                    );
                }
                "--help" | "-h" => {
                    print_rescue_usage();
                    std::process::exit(0);
                }
                other => {
                    bail!("unknown argument: {other}. Run with `rescue --help` for usage.");
                }
            }
            i += 1;
        }

        let rpc_url = rpc_url.ok_or_else(|| eyre::eyre!("--rpc-url is required"))?;

        if seed.is_none() && mnemonic.is_none() {
            bail!("either --seed or --mnemonic is required");
        }

        let funder_key_hex = std::env::var("FUNDER_KEY")
            .map_err(|_| eyre::eyre!("FUNDER_KEY environment variable not set"))?;
        let funder_key: PrivateKeySigner = funder_key_hex.parse()?;

        Ok(Self { rpc_url, seed: seed.unwrap_or(0), scan_count, offset, funder_key, mnemonic })
    }
}

struct DrainParams {
    funder_address: Address,
    chain_id: u64,
    max_fee: u128,
    max_priority_fee: u128,
    drain_gas_cost: U256,
    drain_gas_limit: u128,
    rpc_url: url::Url,
}

async fn run_rescue(raw_args: Vec<String>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = RescueArgs::parse(raw_args)?;

    let client = RpcClient::new(args.rpc_url.clone());
    let chain_id = client.chain_id().await?;
    let funder_address = args.funder_key.address();

    println!("=== Load Test Rescue ===");
    println!("RPC: {} | Chain: {} | Funder: {}", args.rpc_url, chain_id, funder_address);
    println!(
        "Scanning {} accounts (seed={}, offset={})\n",
        args.scan_count, args.seed, args.offset
    );

    let gas_price = client.get_gas_price().await?;
    let max_priority_fee = (gas_price / 10).max(1);
    let max_fee = gas_price.saturating_mul(2).max(max_priority_fee).min(DEFAULT_MAX_GAS_PRICE);
    let drain_gas_limit = 21_000u128;
    let l1_fee_buffer = 1_000_000_000_000_000u128;
    let drain_gas_cost =
        U256::from(drain_gas_limit.saturating_mul(max_fee).saturating_add(l1_fee_buffer));

    let params = DrainParams {
        funder_address,
        chain_id,
        max_fee,
        max_priority_fee,
        drain_gas_cost,
        drain_gas_limit,
        rpc_url: args.rpc_url.clone(),
    };

    let mut total_rescued = U256::ZERO;
    let mut total_accounts_drained = 0usize;
    let mut batch_offset = args.offset;
    let mut remaining = args.scan_count;

    let pb = rescue_progress_bar(args.scan_count as u64, "Scanning accounts");

    while remaining > 0 {
        let batch_count = remaining.min(RESCUE_BATCH_SIZE);

        let accounts = if let Some(ref mnemonic) = args.mnemonic {
            AccountPool::from_mnemonic(mnemonic, batch_count, batch_offset)?
        } else {
            AccountPool::with_offset(args.seed, batch_count, batch_offset)?
        };

        let (rescued, drained) = rescue_batch(&client, &accounts, &params, &pb).await?;

        total_rescued = total_rescued.saturating_add(rescued);
        total_accounts_drained += drained;

        batch_offset += batch_count;
        remaining -= batch_count;
    }

    pb.finish_and_clear();

    println!("\n=== Rescue Complete ===");
    println!(
        "Drained {} accounts | Total rescued: {} ETH",
        total_accounts_drained,
        format_ether(total_rescued)
    );

    Ok(())
}

async fn rescue_batch(
    client: &RpcClient,
    accounts: &AccountPool,
    params: &DrainParams,
    pb: &ProgressBar,
) -> LoadResult<(U256, usize)> {
    let balance_futs: Vec<_> = accounts
        .accounts()
        .iter()
        .map(|a| {
            let client = client.clone();
            let address = a.address;
            async move {
                let balance = client.get_pending_balance(address).await?;
                Ok::<_, BaselineError>((address, balance))
            }
        })
        .collect();

    let balance_results: Vec<_> =
        stream::iter(balance_futs).buffered(RESCUE_CONCURRENCY).collect().await;

    let mut to_drain: Vec<(&FundedAccount, U256)> = Vec::new();
    for (result, account) in balance_results.into_iter().zip(accounts.accounts().iter()) {
        pb.inc(1);
        let (_, balance) = result?;
        if balance > params.drain_gas_cost {
            to_drain.push((account, balance));
        }
    }

    if to_drain.is_empty() {
        return Ok((U256::ZERO, 0));
    }

    let recoverable: U256 = to_drain
        .iter()
        .map(|(_, balance)| balance.saturating_sub(params.drain_gas_cost))
        .fold(U256::ZERO, |a, b| a.saturating_add(b));
    info!(
        accounts = to_drain.len(),
        recoverable_eth = %format_ether(recoverable),
        "found accounts with recoverable balance"
    );

    let drain_futs: Vec<_> = to_drain
        .iter()
        .map(|&(account, balance)| {
            let rpc_url = params.rpc_url.clone();
            let funder_address = params.funder_address;
            let chain_id = params.chain_id;
            let max_fee = params.max_fee;
            let max_priority_fee = params.max_priority_fee;
            let drain_gas_cost = params.drain_gas_cost;
            let drain_gas_limit = params.drain_gas_limit;
            let signer = account.signer.clone();
            let address = account.address;
            async move {
                let send_amount = balance.saturating_sub(drain_gas_cost);
                let wallet = EthereumWallet::from(signer);
                let provider = create_wallet_provider(rpc_url, wallet);
                let nonce = provider
                    .get_transaction_count(address)
                    .pending()
                    .await
                    .map_err(|e| BaselineError::Rpc(e.to_string()))?;

                let tx = TransactionRequest::default()
                    .with_to(funder_address)
                    .with_value(send_amount)
                    .with_nonce(nonce)
                    .with_chain_id(chain_id)
                    .with_gas_limit(drain_gas_limit as u64)
                    .with_max_fee_per_gas(max_fee)
                    .with_max_priority_fee_per_gas(max_priority_fee);

                match provider.send_transaction(tx).await {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        debug!(
                            from = %address,
                            amount = %format_ether(send_amount),
                            tx_hash = %tx_hash,
                            "rescue drain tx sent"
                        );
                        Ok(Some((tx_hash, address, send_amount)))
                    }
                    Err(e) => {
                        warn!(from = %address, error = %e, "rescue drain tx failed, skipping");
                        Ok(None)
                    }
                }
            }
        })
        .collect();

    let drain_results: Vec<_> =
        stream::iter(drain_futs).buffer_unordered(RESCUE_CONCURRENCY).collect().await;

    let mut pending_txs: Vec<(TxHash, Address)> = Vec::new();
    let mut total_drained = U256::ZERO;
    let mut drain_count = 0usize;
    for result in drain_results {
        let result: LoadResult<Option<(TxHash, Address, U256)>> = result;
        if let Some((tx_hash, address, amount)) = result? {
            pending_txs.push((tx_hash, address));
            total_drained = total_drained.saturating_add(amount);
            drain_count += 1;
        }
    }

    if !pending_txs.is_empty() {
        rescue_await_confirmations(client, &mut pending_txs).await?;
    }

    Ok((total_drained, drain_count))
}

async fn rescue_await_confirmations(
    client: &RpcClient,
    pending_txs: &mut Vec<(TxHash, Address)>,
) -> LoadResult<()> {
    let timeout = Duration::from_secs(60);
    let poll_interval = Duration::from_millis(500);
    let start = std::time::Instant::now();

    while !pending_txs.is_empty() && start.elapsed() < timeout {
        tokio::time::sleep(poll_interval).await;

        let receipt_futs: Vec<_> = pending_txs
            .iter()
            .map(|&(tx_hash, address)| {
                let client = client.clone();
                async move {
                    let receipt = client.get_transaction_receipt(tx_hash).await;
                    (tx_hash, address, receipt)
                }
            })
            .collect();

        let receipts: Vec<_> = futures::future::join_all(receipt_futs).await;

        let mut still_pending = Vec::new();
        for (tx_hash, address, receipt) in receipts {
            match receipt {
                Ok(Some(receipt)) => {
                    if receipt.status() {
                        debug!(tx_hash = %tx_hash, address = %address, "rescue tx confirmed");
                    } else {
                        warn!(tx_hash = %tx_hash, address = %address, "rescue tx reverted");
                    }
                }
                Ok(None) => {
                    still_pending.push((tx_hash, address));
                }
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "failed to get receipt");
                    still_pending.push((tx_hash, address));
                }
            }
        }
        *pending_txs = still_pending;
    }

    if !pending_txs.is_empty() {
        let unconfirmed: Vec<_> = pending_txs.iter().map(|(_, addr)| addr).collect();
        warn!(accounts = ?unconfirmed, "some rescue txs did not confirm within timeout");
    }

    Ok(())
}

fn rescue_progress_bar(total: u64, prefix: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("{prefix} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .expect("valid template")
            .progress_chars("█▓░"),
    );
    pb.set_prefix(prefix.to_string());
    pb
}

fn print_rescue_usage() {
    println!(
        "\
Usage: base-load-test rescue --rpc-url <URL> (--seed <SEED> | --mnemonic <PHRASE>) [OPTIONS]

Rescue stranded funds from load test accounts by re-deriving sender
addresses and draining any non-zero balances back to the funder.

Required:
  --rpc-url <URL>        RPC endpoint
  --seed <SEED>          Seed used for account generation
  --mnemonic <PHRASE>    Mnemonic used for account generation (alternative to --seed)

Optional:
  --count <N>            Number of accounts to scan (default: {DEFAULT_RESCUE_SCAN_COUNT})
  --offset <N>           Starting account offset (default: 0)

Environment:
  FUNDER_KEY             Private key of the funder account (hex)"
    );
}
