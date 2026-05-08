//! CLI argument parsing and execution for the load tester binary.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, U256, utils::format_ether};
use alloy_provider::Provider;
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use base_cli_utils::RuntimeManager;
use base_load_tests::{
    AccountPool, BaselineError, FundedAccount, LoadRunner, LoadTestDisplay, MetricsSummary,
    QueryProvider, Result as LoadResult, RpcProviders, RpcResultExt, TestConfig,
    create_wallet_provider,
};
use clap::{ArgGroup, Args, Parser, Subcommand};
use eyre::{Result, bail};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Accounts to derive and check per batch during rescue.
const RESCUE_BATCH_SIZE: usize = 100;

/// Maximum concurrent RPC requests during rescue.
const RESCUE_CONCURRENCY: usize = 32;

/// Default number of accounts to scan during rescue.
const DEFAULT_RESCUE_SCAN_COUNT: usize = 1000;

/// Default maximum gas price (1000 gwei).
const DEFAULT_MAX_GAS_PRICE: u128 = 1_000_000_000_000;

/// The Base load tester CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    about = "Base load tester",
    long_about = None,
    args_conflicts_with_subcommands = true
)]
pub(crate) struct Cli {
    /// Load test arguments.
    #[command(flatten)]
    load: LoadArgs,

    /// Optional subcommand.
    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    /// Runs the selected command.
    pub(crate) fn run(self) -> Result<()> {
        RuntimeManager::new().tokio_runtime()?.block_on(async move {
            match self.command {
                Some(Command::Rescue(args)) => run_rescue(args).await,
                None => run_load_test(self.load).await,
            }
        })
    }
}

/// Load test command arguments.
#[derive(Args, Clone, Debug)]
struct LoadArgs {
    /// Run continuously until interrupted.
    #[arg(long)]
    continuous: bool,

    /// Drain accounts from the configured test set without running a load test.
    #[arg(long)]
    drain_only: bool,

    /// Load test YAML configuration.
    #[arg(value_name = "CONFIG")]
    config: PathBuf,
}

/// Load tester subcommands.
#[derive(Subcommand, Clone, Debug)]
enum Command {
    /// Rescue stranded funds from load test accounts.
    Rescue(RescueArgs),
}

async fn run_load_test(args: LoadArgs) -> Result<()> {
    let mp = LoadTestDisplay::init_tracing();
    let config_path = args.config;

    if !config_path.exists() {
        bail!("config file not found: {}", config_path.display());
    }

    let test_config = TestConfig::load(&config_path)?;

    let query_rpc = test_config
        .query_rpc
        .clone()
        .unwrap_or_else(|| test_config.primary_submission_rpc().expect("validated config").clone());
    let client = RpcProviders::query(query_rpc.clone())?;
    let rpc_chain_id = if test_config.chain_id.is_none() {
        Some(client.get_chain_id().await.rpc("chain id")?)
    } else {
        None
    };

    let load_config = {
        let cfg = test_config.to_load_config(rpc_chain_id)?;
        if args.continuous { cfg.with_continuous() } else { cfg }
    };

    let funding_key = TestConfig::funder_key()?;

    // Drain-only mode: recover funds from a previous interrupted run.
    if args.drain_only {
        println!("=== Drain-Only Mode ===");
        println!(
            "Re-deriving {} accounts from config and draining to funder...",
            load_config.account_count
        );
        let runner = LoadRunner::new(load_config)?;
        match runner.drain_accounts(funding_key).await {
            Ok(drained) => println!("Drained {} ETH back to funder.", format_ether(drained)),
            Err(e) => bail!("drain failed: {e}"),
        }
        return Ok(());
    }

    println!("=== Base Load Test Runner ===");

    println!("Set RPCs to internal endpoints to avoid rate limiting");
    println!(
        "Config: {} | Submit RPCs: {} | Query RPC: {} | Chain: {}",
        config_path.display(),
        test_config
            .transaction_submission_rpcs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        query_rpc,
        load_config.chain_id
    );
    let duration_display =
        load_config.duration.map_or_else(|| "continuous".to_string(), |d| format!("{d:?}"));
    println!(
        "Target: {} GPS | Duration: {} | Accounts: {}",
        load_config.target_gps, duration_display, load_config.account_count
    );
    println!();

    let funding_amount = test_config.parse_funding_amount()?;
    let swap_token_amount = test_config.parse_swap_token_amount()?;

    let config_summary = test_config.to_summary();
    let mut runner = LoadRunner::new(load_config.clone())?;
    runner.set_config_summary(config_summary.clone());

    // Install signal handling before any long-running work so shutdown can
    // stop the run loop and still drain funded accounts.
    let stop_flag = runner.stop_flag();
    install_signal_handler(stop_flag);

    let run_result = run_test_phases(
        &mut runner,
        &funding_key,
        funding_amount,
        swap_token_amount,
        &mp,
        load_config.duration,
    )
    .await;

    let (summary, run_err) = match run_result {
        Ok(summary) => (summary, None),
        Err(e) => {
            let summary = MetricsSummary {
                config: Some(config_summary),
                error: Some(e.to_string()),
                ..Default::default()
            };
            (summary, Some(e))
        }
    };

    if summary.error.is_none() || summary.throughput.total_submitted > 0 {
        println!();
        println!("=== Results ===");
        if let Some(ref err) = summary.error {
            println!("Error: {err}");
        }
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
        let tp = &summary.throughput_percentiles;
        println!(
            "TPS Rolling:   p50={:.0}  p90={:.0}  p99={:.0}  max={:.0}",
            tp.tps_p50, tp.tps_p90, tp.tps_p99, tp.tps_max
        );
        println!(
            "GPS Rolling:   p50={:.0}  p90={:.0}  p99={:.0}  max={:.0}",
            tp.gps_p50, tp.gps_p90, tp.gps_p99, tp.gps_max
        );
        let bl = &summary.block_latency;
        println!(
            "Block Latency: min={:.1?}  p50={:.1?}  mean={:.1?}  p99={:.1?}  max={:.1?}",
            bl.min, bl.p50, bl.mean, bl.p99, bl.max
        );
        let brd = &summary.block_receipt_delay;
        println!(
            "Block Receipt Delay: min={:.1?}  p50={:.1?}  mean={:.1?}  p99={:.1?}  max={:.1?}",
            brd.min, brd.p50, brd.mean, brd.p99, brd.max
        );
        let fb = &summary.flashblocks_latency;
        println!(
            "FB Latency:    min={:.1?}  p50={:.1?}  mean={:.1?}  p99={:.1?}  max={:.1?}  (n={})",
            fb.min, fb.p50, fb.mean, fb.p99, fb.max, fb.count
        );
        println!("Gas: total={}  avg/tx={}", summary.gas.total_gas, summary.gas.avg_gas);
        let br = &summary.block_range;
        match (br.first_block, br.last_block) {
            (Some(first), Some(last)) => {
                println!("Blocks: first={first}  last={last}  span={} block(s)", br.block_count)
            }
            _ => println!("Blocks: no confirmed transactions"),
        }
        if !summary.top_failure_reasons.is_empty() {
            println!("Top failures:");
            for (reason, count) in &summary.top_failure_reasons {
                println!("  {count:>6}x  {reason}");
            }
        }
    } else if let Some(ref err) = summary.error {
        println!();
        println!("=== Error ===");
        println!("{err}");
    }

    if let Ok(output_path) = std::env::var("LOAD_TEST_OUTPUT") {
        match summary.to_json() {
            Ok(json) => match std::fs::write(&output_path, &json) {
                Ok(()) => println!("Results written to {output_path}"),
                Err(e) => eprintln!("Warning: failed to write results to {output_path}: {e}"),
            },
            Err(e) => eprintln!("Warning: failed to serialize results: {e}"),
        }
    }

    // Brief cooldown so in-flight load-test transactions can land and
    // mempool state settles before we query balances for the drain.
    tokio::time::sleep(Duration::from_secs(2)).await;

    println!();
    println!("Draining accounts back to funder...");
    match runner.drain_accounts(funding_key).await {
        Ok(drained) => println!("Drained {} ETH back to funder.", format_ether(drained)),
        Err(e) => eprintln!("Warning: drain failed: {e}"),
    }

    if let Some(e) = run_err {
        return Err(e.into());
    }

    Ok(())
}

/// Runs funding, token setup, and the load test loop, returning the metrics summary.
async fn run_test_phases(
    runner: &mut LoadRunner,
    funding_key: &PrivateKeySigner,
    funding_amount: U256,
    swap_token_amount: U256,
    mp: &indicatif::MultiProgress,
    duration: Option<Duration>,
) -> LoadResult<MetricsSummary> {
    if runner.txpool_node_count() > 0 {
        println!("Clearing txpool sender transactions...");
        let removed = runner.clear_txpools().await?;
        println!("Txpool clearing complete. Removed {removed} transaction(s).");
    }

    println!("Funding test accounts...");
    runner.fund_accounts(funding_key.clone(), funding_amount).await?;
    println!("Accounts funded.");

    if !runner.collect_swap_tokens().is_empty() {
        println!("Distributing swap tokens...");
        runner.setup_swap_tokens(funding_key.clone(), swap_token_amount).await?;
        println!("Swap tokens distributed.");
    }
    println!();

    println!("Running load test...");

    let display = LoadTestDisplay::new(mp, duration);
    runner.set_display(display);

    runner.run().await
}

fn install_signal_handler(stop_flag: Arc<AtomicBool>) {
    let cancel = CancellationToken::new();
    RuntimeManager::install_signal_handler(cancel.clone());

    tokio::spawn(async move {
        cancel.cancelled().await;
        eprintln!("\nReceived signal, stopping gracefully.");
        stop_flag.store(true, Ordering::SeqCst);
    });
}

// ---------------------------------------------------------------------------
// rescue subcommand
// ---------------------------------------------------------------------------

#[derive(Args, Clone, Debug)]
#[command(group(
    ArgGroup::new("account_source")
        .required(true)
        .multiple(false)
        .args(["seed", "mnemonic"])
))]
struct RescueArgs {
    /// RPC endpoint used to scan balances and submit drain transactions.
    #[arg(long = "rpc-url", alias = "rpc", value_name = "URL")]
    rpc_url: url::Url,

    /// Seed used for deterministic account generation.
    #[arg(long)]
    seed: Option<u64>,

    /// Number of accounts to scan.
    #[arg(long = "count", default_value_t = DEFAULT_RESCUE_SCAN_COUNT)]
    scan_count: usize,

    /// Starting account offset.
    #[arg(long, default_value_t = 0)]
    offset: usize,

    /// Mnemonic used for account generation.
    #[arg(long)]
    mnemonic: Option<String>,

    /// Private key of the funder account that receives drained funds.
    #[arg(long = "funder-key", env = "FUNDER_KEY", hide_env_values = true)]
    funder_key: PrivateKeySigner,
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

async fn run_rescue(args: RescueArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let client = RpcProviders::query(args.rpc_url.clone())?;
    let chain_id = client.get_chain_id().await.rpc("chain id")?;
    let funder_address = args.funder_key.address();
    let seed = args.seed.unwrap_or(0);

    println!("=== Load Test Rescue ===");
    println!("RPC: {} | Chain: {} | Funder: {}", args.rpc_url, chain_id, funder_address);
    println!("Scanning {} accounts (seed={}, offset={})\n", args.scan_count, seed, args.offset);

    let gas_price = client.get_gas_price().await.rpc("get gas price")?;
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
            AccountPool::with_offset(seed, batch_count, batch_offset)?
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
    client: &QueryProvider,
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
                let balance = client
                    .get_balance(address)
                    .block_id(BlockNumberOrTag::Pending.into())
                    .await
                    .rpc("get pending balance")?;
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
                    .rpc("get pending transaction count")?;

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
                        Ok(Some((address, send_amount)))
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

    let mut pending_accounts: Vec<Address> = Vec::new();
    let mut total_drained = U256::ZERO;
    let mut drain_count = 0usize;
    for result in drain_results {
        let result: LoadResult<Option<(Address, U256)>> = result;
        if let Some((address, amount)) = result? {
            pending_accounts.push(address);
            total_drained = total_drained.saturating_add(amount);
            drain_count += 1;
        }
    }

    if !pending_accounts.is_empty() {
        rescue_await_drained_balances(client, params.drain_gas_cost, &mut pending_accounts).await?;
    }

    Ok((total_drained, drain_count))
}

async fn rescue_await_drained_balances(
    client: &QueryProvider,
    max_remaining: U256,
    pending_accounts: &mut Vec<Address>,
) -> LoadResult<()> {
    let timeout = Duration::from_secs(60);
    let poll_interval = Duration::from_millis(500);
    let start = std::time::Instant::now();

    while !pending_accounts.is_empty() && start.elapsed() < timeout {
        tokio::time::sleep(poll_interval).await;

        let mut still_pending = Vec::new();
        for address in pending_accounts.drain(..) {
            match client.get_balance(address).await.rpc("get balance") {
                Ok(balance) if balance <= max_remaining => {
                    debug!(address = %address, balance = %balance, "rescue drain balance settled");
                }
                Ok(_) => {
                    still_pending.push(address);
                }
                Err(e) => {
                    warn!(address = %address, error = %e, "failed to check rescue drain balance");
                    still_pending.push(address);
                }
            }
        }
        *pending_accounts = still_pending;
    }

    if !pending_accounts.is_empty() {
        warn!(accounts = ?pending_accounts, "some rescue balances did not settle within timeout");
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
