use std::{
    collections::{BTreeMap, HashMap},
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::transaction::SignableTransaction;
use alloy_eips::Encodable2718;
use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, TxHash, U256, utils::format_ether};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::NonceManager;
use futures::{
    FutureExt as _,
    stream::{self, StreamExt},
};
use indicatif::{ProgressBar, ProgressStyle};
use parking_lot::RwLock;
use revm::precompile::PrecompileId;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

/// Maximum number of concurrent RPC requests during funding/draining operations.
const FUNDING_CONCURRENCY: usize = 32;

/// Maximum number of funding TXs to send before waiting for confirmation.
/// Kept below typical per-sender txpool limits (e.g. reth default is 16) to
/// avoid "txpool is full" rejections when all TXs originate from one funder.
const FUNDING_BATCH_SIZE: usize = 16;

use super::{
    AdaptiveBackoff, BlockFirstSeen, BlockWatcher, Confirmer, ConfirmerHandle, DisplaySnapshot,
    FlashblockTimes, FlashblockTracker, LoadConfig, LoadTestDisplay, RateLimiter, TxType,
};
use crate::{
    BaselineError, Result,
    config::{OsakaTarget, WorkloadConfig},
    metrics::{MetricsCollector, MetricsSummary, TransactionMetrics},
    rpc::{BatchRpcClient, BatchSendResult, RpcClient, WalletProvider, create_wallet_provider},
    workload::{
        AccountPool, CalldataPayload, Erc20Payload, OsakaPayload, PrecompilePayload,
        TransferPayload, WorkloadGenerator,
    },
};

/// Provider type for nonce management. Uses Ethereum network type because
/// `NonceManager` only calls `get_transaction_count`, which returns the same
/// response for both Ethereum and Base networks.
type NonceProvider = RootProvider<Ethereum>;

struct PreparedTx {
    from: Address,
    to: Option<Address>,
    value: U256,
    data: Bytes,
    gas_limit: u64,
}

enum SubmitEvent {
    Submitted(TxHash),
    Failed(String),
}

struct SignedTx {
    raw: Bytes,
    tx_hash: TxHash,
    from: Address,
    nonce: u64,
}

/// Shared context cloned into each `submit_batch` spawned task.
#[derive(Clone)]
struct BatchSubmitCtx {
    providers: Arc<HashMap<Address, WalletProvider>>,
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    batch_rpc: BatchRpcClient,
    nonce_managers: Arc<HashMap<Address, NonceManager<NonceProvider>>>,
    confirmer_handle: ConfirmerHandle,
    submit_event_tx: mpsc::Sender<SubmitEvent>,
    gas_price: u128,
    chain_id: u64,
    max_gas_price: u128,
}

const NONCE_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Warn when any account drops below 0.001 ETH.
const LOW_BALANCE_THRESHOLD: u128 = 1_000_000_000_000_000;

/// Executes load tests by generating and submitting transactions at a target rate.
pub struct LoadRunner {
    config: LoadConfig,
    client: RpcClient,
    accounts: AccountPool,
    generator: WorkloadGenerator,
    collector: MetricsCollector,
    stop_flag: Arc<AtomicBool>,
    cancel_token: CancellationToken,
    nonce_managers: Arc<HashMap<Address, NonceManager<NonceProvider>>>,
    providers: Arc<HashMap<Address, WalletProvider>>,
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    batch_rpc: BatchRpcClient,
    gas_price: u128,
    display: Option<LoadTestDisplay>,
    snapshot_tx: Option<watch::Sender<DisplaySnapshot>>,
    last_total_eth: Option<String>,
    last_min_eth: Option<String>,
    last_funds_low: bool,
    funder_address: Option<String>,
    sender_addresses: Vec<String>,
}

impl LoadRunner {
    /// Creates a new load runner with the given configuration.
    #[instrument(skip_all, fields(rpc_url = %config.rpc_http_url, chain_id = config.chain_id))]
    pub fn new(config: LoadConfig) -> Result<Self> {
        config.validate()?;

        let client = RpcClient::new(config.rpc_http_url.clone());

        let accounts = if let Some(mnemonic) = &config.mnemonic {
            info!(
                offset = config.sender_offset,
                count = config.account_count,
                "deriving accounts from mnemonic"
            );
            AccountPool::from_mnemonic(mnemonic, config.account_count, config.sender_offset)?
        } else {
            info!(
                seed = config.seed,
                offset = config.sender_offset,
                count = config.account_count,
                "generating accounts from seed"
            );
            AccountPool::with_offset(config.seed, config.account_count, config.sender_offset)?
        };

        let providers = Arc::new(Self::build_providers(&config.rpc_http_url, &accounts));
        let signers = Arc::new(Self::build_signers(&accounts));
        let batch_rpc = BatchRpcClient::new(config.rpc_http_url.clone());
        let sender_addresses = accounts.accounts().iter().map(|a| a.address.to_string()).collect();

        let workload_config = WorkloadConfig::new("load-test").with_seed(config.seed);
        let generator = Self::create_generator(workload_config, &config)?;

        info!(
            account_count = config.account_count,
            providers_cached = providers.len(),
            "load runner created with cached providers"
        );

        Ok(Self {
            config,
            client,
            accounts,
            generator,
            collector: MetricsCollector::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
            nonce_managers: Arc::new(HashMap::new()),
            providers,
            signers,
            batch_rpc,
            gas_price: 0,
            display: None,
            snapshot_tx: None,
            last_total_eth: None,
            last_min_eth: None,
            last_funds_low: false,
            funder_address: None,
            sender_addresses,
        })
    }

    /// Sets the funder wallet address for inclusion in live snapshots.
    pub fn set_funder_address(&mut self, addr: String) {
        self.funder_address = Some(addr);
    }

    fn build_providers(
        rpc_url: &url::Url,
        accounts: &AccountPool,
    ) -> HashMap<Address, WalletProvider> {
        let mut providers = HashMap::with_capacity(accounts.len());

        for account in accounts.accounts() {
            let wallet = EthereumWallet::from(account.signer.clone());
            let provider = create_wallet_provider(rpc_url.clone(), wallet);
            providers.insert(account.address, provider);
        }

        providers
    }

    fn build_signers(accounts: &AccountPool) -> HashMap<Address, PrivateKeySigner> {
        accounts
            .accounts()
            .iter()
            .map(|a| (a.address, a.signer.clone()))
            .collect()
    }

    fn create_generator(
        workload_config: WorkloadConfig,
        config: &LoadConfig,
    ) -> Result<WorkloadGenerator> {
        let mut generator = WorkloadGenerator::new(workload_config);

        let total_weight: u32 = config.transactions.iter().map(|t| t.weight).sum();
        if total_weight == 0 {
            return Err(BaselineError::Config("total transaction weight must be > 0".into()));
        }

        for tx_config in &config.transactions {
            let weight_pct = (tx_config.weight as f64 / total_weight as f64) * 100.0;

            match &tx_config.tx_type {
                TxType::Transfer => {
                    generator = generator.with_payload(TransferPayload::default(), weight_pct);
                }
                TxType::Calldata { max_size, repeat_count } => {
                    let payload = CalldataPayload::new(*max_size).with_repeat_count(*repeat_count);
                    generator = generator.with_payload(payload, weight_pct);
                }
                TxType::Erc20 { contract } => {
                    generator = generator.with_payload(
                        Erc20Payload::new(*contract, U256::from(1000), U256::from(10000)),
                        weight_pct,
                    );
                }
                TxType::Precompile { target, blake2f_rounds, iterations, looper_contract } => {
                    let payload = PrecompilePayload::with_options(
                        target.clone(),
                        *blake2f_rounds,
                        *iterations,
                        *looper_contract,
                    );
                    generator = generator.with_payload(payload, weight_pct);
                }
                TxType::Osaka { target } => {
                    generator =
                        generator.with_payload(OsakaPayload::new(target.clone()), weight_pct);
                }
            }
        }

        Ok(generator)
    }

    fn estimate_avg_gas(&self) -> u64 {
        let total_weight: u32 = self.config.transactions.iter().map(|t| t.weight).sum();
        if total_weight == 0 {
            return 21_000;
        }

        let mut weighted_gas = 0u64;
        for tx_config in &self.config.transactions {
            // Estimates actual gas_used (not gas_limit). For precompiles,
            // execution cost on small inputs is negligible compared to
            // the 21K intrinsic + calldata overhead, so the estimate is
            // much lower than the generous gas_limit set on the tx.
            let gas_estimate = match &tx_config.tx_type {
                TxType::Transfer => 21_000,
                TxType::Calldata { max_size, .. } => 21_000 + (*max_size as u64 * 16),
                TxType::Erc20 { .. } => 65_000,
                TxType::Precompile { target, iterations, blake2f_rounds, .. } => {
                    let per_call = match target {
                        PrecompileId::Identity | PrecompileId::Bn254Add => 22_000,
                        PrecompileId::Sha256 | PrecompileId::Ripemd160 => 23_000,
                        PrecompileId::Bn254Mul => 28_000,
                        PrecompileId::ModExp => 30_000,
                        PrecompileId::Bn254Pairing => 45_000,
                        PrecompileId::Blake2F => {
                            30_000 + u64::from(blake2f_rounds.unwrap_or(1_000))
                        }
                        PrecompileId::KzgPointEvaluation => 55_000,
                        _ => 25_000,
                    };
                    if *iterations > 1 {
                        50_000 + per_call * (*iterations as u64)
                    } else {
                        per_call
                    }
                }
                TxType::Osaka { target } => match target {
                    OsakaTarget::Clz => 80_000,
                    OsakaTarget::P256verifyOsaka | OsakaTarget::ModexpOsaka => 30_000,
                },
            };
            weighted_gas += gas_estimate * tx_config.weight as u64;
        }

        weighted_gas / total_weight as u64
    }

    /// Funds all accounts from a funding key up to the specified amount.
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn fund_accounts(
        &mut self,
        funding_key: PrivateKeySigner,
        amount_per_account: U256,
    ) -> Result<()> {
        let total_accounts = self.accounts.len();
        let client = self.client.clone();
        let rpc_url = self.config.rpc_http_url.clone();
        let chain_id = self.config.chain_id;
        let max_gas_price = self.config.max_gas_price;

        let pb_check = self.progress_bar(total_accounts as u64, "Checking balances");

        // Phase 1: Parallel balance + nonce queries.
        let addresses: Vec<(Address, usize)> =
            self.accounts.accounts().iter().enumerate().map(|(i, a)| (a.address, i)).collect();

        let balance_futs: Vec<_> = addresses
            .iter()
            .map(|&(addr, idx)| {
                let client = client.clone();
                async move {
                    let balance = client.get_balance(addr).await?;
                    let nonce = client.get_nonce(addr).await?;
                    Ok::<_, BaselineError>((addr, idx, balance, nonce))
                }
            })
            .collect();

        let results: Vec<_> = stream::iter(balance_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_check.inc(1))
            .collect()
            .await;
        pb_check.finish_and_clear();

        let mut accounts_to_fund = Vec::new();
        for result in results {
            let (addr, idx, balance, nonce) = result?;
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = nonce;

            if balance < amount_per_account {
                let deficit = amount_per_account.saturating_sub(balance);
                accounts_to_fund.push((addr, deficit));
            } else {
                debug!(address = %addr, balance = %balance, "account already funded");
            }
        }

        if accounts_to_fund.is_empty() {
            info!("all accounts already have sufficient balance, skipping funding");
            return Ok(());
        }

        let funder_address = funding_key.address();
        let wallet = EthereumWallet::from(funding_key);
        let funder_provider = Arc::new(create_wallet_provider(rpc_url.clone(), wallet));

        let gas_price = client.get_gas_price().await?;
        let max_priority_fee = (gas_price / 10).max(1);
        // Ensure max_fee >= max_priority_fee (EIP-1559 requirement).
        // When gas_price is 0 (e.g. a fresh devnet), `gas_price * 2` would be 0
        // while max_priority_fee=1, causing the transaction to be rejected.
        let max_fee = gas_price.saturating_mul(2).max(max_priority_fee).min(max_gas_price);

        // Phase 2: Early balance validation — abort before sending any TXs if
        // the funder cannot cover the total cost.
        let total_deficit: U256 = accounts_to_fund
            .iter()
            .map(|(_, deficit)| *deficit)
            .fold(U256::ZERO, |a, b| a.saturating_add(b));
        let gas_cost_per_tx = U256::from(21_000u64).saturating_mul(U256::from(max_fee));
        let total_gas_cost = gas_cost_per_tx.saturating_mul(U256::from(accounts_to_fund.len()));
        let total_needed = total_deficit.saturating_add(total_gas_cost);

        let funder_balance = client.get_balance(funder_address).await?;

        if funder_balance < total_needed {
            let shortfall = total_needed.saturating_sub(funder_balance);
            return Err(BaselineError::Transaction(format!(
                "funder {} has insufficient balance: has {} ETH, needs {} ETH (deficit {} ETH + gas {} ETH), shortfall {} ETH",
                funder_address,
                format_ether(funder_balance),
                format_ether(total_needed),
                format_ether(total_deficit),
                format_ether(total_gas_cost),
                format_ether(shortfall),
            )));
        }

        let start_nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        info!(
            from = %funder_address,
            amount = %amount_per_account,
            accounts_needing_funds = accounts_to_fund.len(),
            funder_balance = %format_ether(funder_balance),
            total_needed = %format_ether(total_needed),
            "funding accounts"
        );

        let replacement_max_fee = max_fee.saturating_mul(3);
        let replacement_priority_fee = max_priority_fee.saturating_mul(3);

        // Phase 3+4: Send funding TXs in batches and confirm each batch before
        // sending the next. This avoids overwhelming the txpool's per-sender limit.
        let txs: Vec<(TransactionRequest, Address, U256, u64)> = accounts_to_fund
            .iter()
            .enumerate()
            .map(|(i, &(address, deficit))| {
                let nonce = start_nonce
                    .checked_add(u64::try_from(i).expect("account index exceeds u64"))
                    .expect("nonce overflow");
                let tx = TransactionRequest::default()
                    .with_to(address)
                    .with_value(deficit)
                    .with_nonce(nonce)
                    .with_chain_id(chain_id)
                    .with_gas_limit(21_000)
                    .with_max_fee_per_gas(max_fee)
                    .with_max_priority_fee_per_gas(max_priority_fee);
                (tx, address, deficit, nonce)
            })
            .collect();

        let total_txs = txs.len() as u64;
        let pb_fund = self.progress_bar(total_txs, "Funding accounts");
        let mut txs_remaining = txs.into_iter().peekable();
        while txs_remaining.peek().is_some() {
            let batch: Vec<_> = txs_remaining.by_ref().take(FUNDING_BATCH_SIZE).collect();
            let mut batch_pending: Vec<(TxHash, Address)> = Vec::with_capacity(batch.len());
            let mut retries: Vec<(Address, U256, u64)> = Vec::new();
            let mut fatal_errors: Vec<String> = Vec::new();

            let send_futs = batch.into_iter().map(|(tx, address, deficit, nonce)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let result = provider.send_transaction(tx).await;
                    (result, address, deficit, nonce)
                }
            });

            let mut send_stream = stream::iter(send_futs).buffer_unordered(FUNDING_BATCH_SIZE);

            while let Some((result, address, deficit, nonce)) = send_stream.next().await {
                match result {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        debug!(to = %address, deficit = %deficit, nonce, tx_hash = %tx_hash, "funding tx sent");
                        batch_pending.push((tx_hash, address));
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        if error_str.contains("already known") {
                            retries.push((address, deficit, nonce));
                        } else {
                            error!(to = %address, error = %e, "failed to fund account");
                            fatal_errors.push(format!("failed to fund {address}: {e}"));
                        }
                    }
                }
            }

            if !fatal_errors.is_empty() {
                pb_fund.finish_and_clear();
                return Err(BaselineError::Transaction(format!(
                    "{} funding tx(s) failed: {}",
                    fatal_errors.len(),
                    fatal_errors.join("; "),
                )));
            }

            if !retries.is_empty() {
                let retry_futs = retries.into_iter().map(|(address, deficit, nonce)| {
                    let provider = Arc::clone(&funder_provider);
                    async move {
                        let replacement = TransactionRequest::default()
                            .with_to(address)
                            .with_value(deficit)
                            .with_nonce(nonce)
                            .with_chain_id(chain_id)
                            .with_gas_limit(21_000)
                            .with_max_fee_per_gas(replacement_max_fee)
                            .with_max_priority_fee_per_gas(replacement_priority_fee);
                        let result = provider.send_transaction(replacement).await;
                        (result, address, nonce)
                    }
                });

                let mut retry_stream =
                    stream::iter(retry_futs).buffer_unordered(FUNDING_BATCH_SIZE);

                while let Some((result, address, nonce)) = retry_stream.next().await {
                    match result {
                        Ok(pending) => {
                            let tx_hash = *pending.tx_hash();
                            info!(to = %address, nonce, tx_hash = %tx_hash, "replacement funding tx sent");
                            batch_pending.push((tx_hash, address));
                        }
                        Err(replace_err) => {
                            warn!(to = %address, nonce, error = %replace_err, "replacement tx also failed, proceeding");
                        }
                    }
                }
            }

            Self::await_confirmations(&client, &mut batch_pending, &pb_fund).await?;
        }
        pb_fund.finish_and_clear();

        // Phase 5: Parallel post-funding state refresh.
        let pb_refresh = self.progress_bar(total_accounts as u64, "Refreshing account state");
        let refresh_futs: Vec<_> = self
            .accounts
            .accounts()
            .iter()
            .map(|a| {
                let client = client.clone();
                let addr = a.address;
                async move {
                    let balance = client.get_balance(addr).await?;
                    let nonce = client.get_nonce(addr).await?;
                    Ok::<_, BaselineError>((addr, balance, nonce))
                }
            })
            .collect();

        let refresh_results: Vec<_> = stream::iter(refresh_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_refresh.inc(1))
            .collect()
            .await;
        pb_refresh.finish_and_clear();

        let addr_to_idx: HashMap<Address, usize> =
            self.accounts.accounts().iter().enumerate().map(|(i, a)| (a.address, i)).collect();

        for result in refresh_results {
            let (addr, balance, account_nonce) = result?;
            let idx = addr_to_idx[&addr];
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = account_nonce;

            let provider = NonceProvider::new_http(self.config.rpc_http_url.clone());
            let nonce_manager =
                NonceManager::new(provider, addr, NONCE_RPC_TIMEOUT).with_pending_tag();
            Arc::make_mut(&mut self.nonce_managers).insert(addr, nonce_manager);

            debug!(address = %addr, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        info!(funded = accounts_to_fund.len(), "funding complete");
        Ok(())
    }

    /// Runs the load test and returns metrics summary.
    #[instrument(skip(self), fields(target_gps = self.config.target_gps, continuous = self.config.duration.is_none(), duration = ?self.config.duration))]
    pub async fn run(&mut self) -> Result<MetricsSummary> {
        self.collector.reset();
        self.stop_flag.store(false, Ordering::SeqCst);
        self.cancel_token = CancellationToken::new();

        self.gas_price = self.client.get_gas_price().await?;
        info!(gas_price = self.gas_price, "fetched current gas price");

        for account in self.accounts.accounts() {
            if !self.nonce_managers.contains_key(&account.address) {
                let provider = NonceProvider::new_http(self.config.rpc_http_url.clone());
                let nonce_manager =
                    NonceManager::new(provider, account.address, NONCE_RPC_TIMEOUT)
                        .with_pending_tag();
                Arc::make_mut(&mut self.nonce_managers).insert(account.address, nonce_manager);
            }
        }

        for (address, nonce_manager) in self.nonce_managers.iter() {
            match nonce_manager.next_nonce().await {
                Ok(guard) => {
                    guard.rollback();
                    debug!(address = %address, "nonce manager pre-warmed");
                }
                Err(e) => {
                    warn!(address = %address, error = %e, "failed to pre-warm nonce manager");
                }
            }
        }

        const METRICS_CHANNEL_BUFFER: usize = 2000;
        const SUBMIT_CHANNEL_BUFFER: usize = 2000;
        const SUBMIT_BATCH_CONCURRENCY: usize = 32;
        let (metrics_tx, mut metrics_rx) =
            mpsc::channel::<TransactionMetrics>(METRICS_CHANNEL_BUFFER);
        let (submit_event_tx, mut submit_event_rx) =
            mpsc::channel::<SubmitEvent>(SUBMIT_CHANNEL_BUFFER);

        let flashblock_times: FlashblockTimes = Arc::new(RwLock::new(HashMap::new()));
        let block_first_seen: BlockFirstSeen = Arc::new(RwLock::new(BTreeMap::new()));

        let flashblock_tracker_task = if let Some(url) = &self.config.flashblocks_ws_url {
            info!(url = %url, "starting flashblock tracker");
            Some(
                FlashblockTracker::new(
                    url.clone(),
                    Arc::clone(&flashblock_times),
                    self.cancel_token.clone(),
                )
                .start(),
            )
        } else {
            info!("flashblocks_ws_url not configured, flashblock latency tracking disabled");
            None
        };

        let block_watcher_enabled = self.config.block_watcher_url.is_some();
        let block_watcher_task = if let Some(url) = &self.config.block_watcher_url {
            info!(url = %url, "starting block watcher");
            Some(
                BlockWatcher::new(
                    url.clone(),
                    Arc::clone(&block_first_seen),
                    self.cancel_token.clone(),
                )
                .start(),
            )
        } else {
            info!("block_watcher_url not configured, using block timestamps for latency");
            None
        };

        let sender_addresses: Vec<_> = self.accounts.accounts().iter().map(|a| a.address).collect();
        let mut confirmer = Confirmer::new(
            &sender_addresses,
            metrics_tx,
            Arc::clone(&self.stop_flag),
            Arc::clone(&flashblock_times),
            Arc::clone(&block_first_seen),
            block_watcher_enabled,
            self.batch_rpc.clone(),
        );
        let confirmer_handle = confirmer.handle();
        let confirmer_handle_for_run = confirmer_handle.clone();

        let confirmer_client = RpcClient::new(self.config.rpc_http_url.clone());
        let confirmer_task = tokio::spawn(async move {
            confirmer.run(confirmer_client, &confirmer_handle_for_run).await
        });

        let max_in_flight_per_sender = self.config.max_in_flight_per_sender;

        let initial_avg_gas = self.estimate_avg_gas();
        let mut rate_limiter = RateLimiter::new(self.config.target_gps, initial_avg_gas);
        let start = Instant::now();
        let mut current_account_idx = 0usize;
        let account_count = self.accounts.len();

        let batch_size = self.config.batch_size;
        let batch_timeout = self.config.batch_timeout;

        info!(
            target_gps = self.config.target_gps,
            initial_avg_gas,
            effective_tps = rate_limiter.effective_tps(),
            max_in_flight_per_sender,
            batch_size,
            batch_timeout_ms = batch_timeout.as_millis(),
            "starting load test with per-sender in-flight limiting"
        );

        let mut pending_batch: Vec<PreparedTx> = Vec::with_capacity(batch_size);
        let semaphore = Arc::new(Semaphore::new(SUBMIT_BATCH_CONCURRENCY));
        let providers = Arc::clone(&self.providers);
        let signers = Arc::clone(&self.signers);
        let nonce_managers = Arc::clone(&self.nonce_managers);
        let batch_rpc = self.batch_rpc.clone();
        let mut last_gas_price_refresh = Instant::now();
        let mut last_rate_limiter_update = Instant::now();
        let mut last_progress_report = Instant::now();
        let mut last_balance_check = Instant::now();
        const GAS_PRICE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
        const RATE_LIMITER_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
        const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
        const DISPLAY_RENDER_INTERVAL: Duration = Duration::from_millis(500);
        const BALANCE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

        let use_live_display = self.display.as_ref().is_some_and(|d| d.is_active());
        let use_snapshot_tx = self.snapshot_tx.is_some();

        self.check_account_balances().await;

        // Emit an initial snapshot immediately so the TUI renders live
        // metrics (submitted/in-flight/failed counters) without waiting
        // for the first confirmation to arrive.
        if use_live_display || use_snapshot_tx {
            let snap = self.build_snapshot(
                start,
                &confirmer_handle,
                max_in_flight_per_sender,
                account_count,
            );
            if let Some(ref d) = self.display {
                d.update(&snap);
            }
            if let Some(ref tx) = self.snapshot_tx {
                let _ = tx.send(snap);
            }
        }

        while self.config.duration.is_none_or(|d| start.elapsed() < d)
            && !self.stop_flag.load(Ordering::SeqCst)
        {
            // --- Housekeeping (runs once per batch iteration) ---

            if last_gas_price_refresh.elapsed() >= GAS_PRICE_REFRESH_INTERVAL {
                if let Ok(new_price) = self.client.get_gas_price().await
                    && new_price != self.gas_price
                {
                    debug!(old_price = self.gas_price, new_price, "gas price updated");
                    self.gas_price = new_price;
                }
                last_gas_price_refresh = Instant::now();
            }

            if last_rate_limiter_update.elapsed() >= RATE_LIMITER_UPDATE_INTERVAL {
                if let Some(avg_gas) = self.collector.avg_gas_used() {
                    rate_limiter.update_avg_gas(avg_gas);
                }
                last_rate_limiter_update = Instant::now();
            }

            if last_balance_check.elapsed() >= BALANCE_CHECK_INTERVAL {
                self.check_account_balances().await;
                last_balance_check = Instant::now();
            }

            // Drain confirmed metrics non-blocking so the rolling window stays
            // current during the run (not just during the post-run drain).
            while let Ok(metrics) = metrics_rx.try_recv() {
                self.collector.record_confirmed(metrics);
            }

            while let Ok(event) = submit_event_rx.try_recv() {
                match event {
                    SubmitEvent::Submitted(tx_hash) => self.collector.record_submitted(tx_hash),
                    SubmitEvent::Failed(reason) => {
                        self.collector.record_failed(TxHash::ZERO, &reason);
                    }
                }
            }

            if use_live_display || use_snapshot_tx {
                if last_progress_report.elapsed() >= DISPLAY_RENDER_INTERVAL {
                    let snap = self.build_snapshot(
                        start,
                        &confirmer_handle,
                        max_in_flight_per_sender,
                        account_count,
                    );
                    if let Some(ref d) = self.display {
                        d.update(&snap);
                    }
                    if let Some(ref tx) = self.snapshot_tx {
                        let _ = tx.send(snap);
                    }
                    last_progress_report = Instant::now();
                }
            } else if last_progress_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
                let elapsed_secs = start.elapsed().as_secs();
                let submitted = self.collector.submitted_count();
                let confirmed = self.collector.confirmed_count();
                let failed = self.collector.failed_count();
                let in_flight = confirmer_handle.total_in_flight();
                let senders_blocked = confirmer_handle.senders_at_limit(max_in_flight_per_sender);
                let (p50, p99) = self.collector.rolling_p50_p99();
                let (flashblocks_p50, flashblocks_p99) =
                    self.collector.rolling_flashblocks_p50_p99();
                info!(
                    elapsed_secs,
                    submitted,
                    confirmed,
                    failed,
                    in_flight,
                    senders_blocked,
                    gas_price = self.gas_price,
                    p50_ms = p50.as_millis() as u64,
                    p99_ms = p99.as_millis() as u64,
                    flashblocks_p50_ms = flashblocks_p50.as_millis() as u64,
                    flashblocks_p99_ms = flashblocks_p99.as_millis() as u64,
                    "progress"
                );
                last_progress_report = Instant::now();
            }

            // --- Inner loop: fill batch without sleeping ---

            let batch_start = Instant::now();
            let mut consecutive_at_limit = 0usize;

            while pending_batch.len() < batch_size && batch_start.elapsed() < batch_timeout {
                let account = &self.accounts.accounts()[current_account_idx];
                let sender_in_flight = confirmer_handle.in_flight_for(&account.address);

                if sender_in_flight >= max_in_flight_per_sender {
                    debug!(
                        sender = %account.address,
                        in_flight = sender_in_flight,
                        max = max_in_flight_per_sender,
                        "sender in-flight limit reached, skipping to next"
                    );
                    current_account_idx = (current_account_idx + 1) % account_count;
                    consecutive_at_limit += 1;

                    if consecutive_at_limit >= account_count {
                        // All senders at limit — break out and flush whatever we have.
                        break;
                    }
                    continue;
                }

                consecutive_at_limit = 0;

                let from = account.address;
                let to_idx = (current_account_idx + 1) % account_count;
                let to = self.accounts.accounts()[to_idx].address;

                let tx_request = self.generator.generate_payload(from, to)?;

                let to_addr = tx_request.to.and_then(|kind| kind.to().copied());
                let value = tx_request.value.unwrap_or(U256::ZERO);
                let data = tx_request.input.input().cloned().unwrap_or_default();
                let gas_limit = tx_request.gas.unwrap_or(21_000);

                pending_batch.push(PreparedTx { from, to: to_addr, value, data, gas_limit });

                current_account_idx = (current_account_idx + 1) % account_count;
            }

            // --- Batch-level rate limiting and submission ---

            if pending_batch.is_empty() {
                // All senders blocked — backpressure sleep to avoid busy-spin.
                tokio::time::sleep(Duration::from_millis(10)).await;
                rate_limiter.reset_tick();
                continue;
            }

            rate_limiter.tick_batch(pending_batch.len()).await;

            let batch = std::mem::replace(&mut pending_batch, Vec::with_capacity(batch_size));
            let permit = Arc::clone(&semaphore)
                .acquire_owned()
                .await
                .expect("submit batch semaphore closed");
            let ctx = BatchSubmitCtx {
                providers: Arc::clone(&providers),
                signers: Arc::clone(&signers),
                batch_rpc: batch_rpc.clone(),
                nonce_managers: Arc::clone(&nonce_managers),
                confirmer_handle: confirmer_handle.clone(),
                submit_event_tx: submit_event_tx.clone(),
                gas_price: self.gas_price,
                chain_id: self.config.chain_id,
                max_gas_price: self.config.max_gas_price,
            };
            tokio::spawn(async move {
                let _permit = permit;
                match AssertUnwindSafe(Self::submit_batch(ctx, batch)).catch_unwind().await {
                    Ok(submitted) => debug!(submitted, "batch submitted"),
                    Err(_) => error!("batch submission task panicked"),
                }
            });
        }

        let active_duration = start.elapsed();

        if !pending_batch.is_empty() {
            let permit = Arc::clone(&semaphore)
                .acquire_owned()
                .await
                .expect("submit batch semaphore closed");
            let ctx = BatchSubmitCtx {
                providers: Arc::clone(&providers),
                signers: Arc::clone(&signers),
                batch_rpc: batch_rpc.clone(),
                nonce_managers: Arc::clone(&nonce_managers),
                confirmer_handle: confirmer_handle.clone(),
                submit_event_tx: submit_event_tx.clone(),
                gas_price: self.gas_price,
                chain_id: self.config.chain_id,
                max_gas_price: self.config.max_gas_price,
            };
            tokio::spawn(async move {
                let _permit = permit;
                match AssertUnwindSafe(Self::submit_batch(ctx, pending_batch)).catch_unwind().await
                {
                    Ok(submitted) => debug!(submitted, "final batch submitted"),
                    Err(_) => error!("final batch submission task panicked"),
                }
            });
        }

        let all_permits = Arc::clone(&semaphore)
            .acquire_many_owned(SUBMIT_BATCH_CONCURRENCY as u32)
            .await
            .expect("submit batch semaphore closed");
        drop(all_permits);

        // Close the channel so the drain below cannot miss late events.
        drop(submit_event_tx);

        while let Ok(event) = submit_event_rx.try_recv() {
            match event {
                SubmitEvent::Submitted(tx_hash) => self.collector.record_submitted(tx_hash),
                SubmitEvent::Failed(reason) => {
                    self.collector.record_failed(TxHash::ZERO, &reason);
                }
            }
        }

        self.stop_flag.store(true, Ordering::SeqCst);

        if let Some(display) = &self.display {
            display.finish();
        }

        let submitted = self.collector.submitted_count();
        let in_flight = confirmer_handle.total_in_flight();
        let elapsed = start.elapsed();
        info!(
            submitted,
            in_flight,
            elapsed_secs = elapsed.as_secs(),
            actual_tps = submitted as f64 / elapsed.as_secs_f64(),
            "load test complete, draining confirmations"
        );

        let drain_timeout = Duration::from_secs(60);
        let drain_start = Instant::now();
        let confirmer_poll_interval_ms = 600; // Slightly longer than confirmer's 500ms poll

        while drain_start.elapsed() < drain_timeout {
            match tokio::time::timeout(
                Duration::from_millis(confirmer_poll_interval_ms),
                metrics_rx.recv(),
            )
            .await
            {
                Ok(Some(metrics)) => {
                    self.collector.record_confirmed(metrics);
                }
                Ok(None) => break,
                Err(_) if confirmer_task.is_finished() => {
                    while let Ok(metrics) = metrics_rx.try_recv() {
                        self.collector.record_confirmed(metrics);
                    }
                    break;
                }
                Err(_) => continue,
            }
        }

        // Let the confirmer finish gracefully (stop_flag is already set).
        // Block watcher stays alive so deferred block latencies can still resolve.
        if tokio::time::timeout(Duration::from_secs(2), confirmer_task).await.is_err() {
            warn!("confirmer did not shut down in time");
        }

        while let Ok(metrics) = metrics_rx.try_recv() {
            self.collector.record_confirmed(metrics);
        }

        // Now safe to stop WebSocket tasks — confirmer is done.
        self.cancel_token.cancel();

        if let Some(task) = flashblock_tracker_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "flashblock tracker panicked"),
                _ => {}
            }
        }
        if let Some(task) = block_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "block watcher panicked"),
                _ => {}
            }
        }

        let expired = confirmer_handle.expired_count();
        if expired > 0 {
            self.collector.record_failures("expired without confirmation", expired);
        }

        let confirmed = self.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        Ok(self.collector.summarize(active_duration))
    }

    fn build_snapshot(
        &mut self,
        start: Instant,
        confirmer_handle: &ConfirmerHandle,
        max_in_flight_per_sender: u64,
        account_count: usize,
    ) -> DisplaySnapshot {
        let (p50, p99) = self.collector.rolling_p50_p99();
        let (flashblocks_p50, flashblocks_p99) = self.collector.rolling_flashblocks_p50_p99();
        DisplaySnapshot {
            elapsed: start.elapsed(),
            duration: self.config.duration,
            submitted: self.collector.submitted_count(),
            confirmed: self.collector.confirmed_count(),
            failed: self.collector.failed_count(),
            in_flight: confirmer_handle.total_in_flight(),
            senders_blocked: confirmer_handle.senders_at_limit(max_in_flight_per_sender),
            total_senders: account_count,
            rolling_tps: self.collector.rolling_tps(),
            rolling_gps: self.collector.rolling_gps(),
            p50_latency: p50,
            p99_latency: p99,
            flashblocks_p50_latency: flashblocks_p50,
            flashblocks_p99_latency: flashblocks_p99,
            gas_price_gwei: self.gas_price as f64 / 1e9,
            total_eth: self.last_total_eth.clone(),
            min_eth: self.last_min_eth.clone(),
            funds_low: self.last_funds_low,
            funder_address: self.funder_address.clone(),
            sender_addresses: self.sender_addresses.clone(),
        }
    }

    async fn submit_batch(ctx: BatchSubmitCtx, batch: Vec<PreparedTx>) -> u64 {
        let max_fee = ctx.gas_price.saturating_mul(2).min(ctx.max_gas_price);
        let priority_fee = (ctx.gas_price / 10).max(1);
        let chain_id = ctx.chain_id;

        // Phase 1: Acquire nonces and sign all transactions locally.
        let mut signed_txs: Vec<SignedTx> = Vec::with_capacity(batch.len());

        for prepared in &batch {
            let Some(signer) = ctx.signers.get(&prepared.from) else {
                warn!(from = %prepared.from, "no signer for sender");
                let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("no signer".into())).await;
                continue;
            };

            let Some(nonce_manager) = ctx.nonce_managers.get(&prepared.from) else {
                warn!(from = %prepared.from, "no nonce manager for sender");
                let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("no nonce manager".into())).await;
                continue;
            };

            let nonce_guard = match nonce_manager.next_nonce().await {
                Ok(guard) => guard,
                Err(e) => {
                    warn!(from = %prepared.from, error = %e, "failed to acquire nonce");
                    let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("nonce acquisition failed".into())).await;
                    continue;
                }
            };
            let nonce = nonce_guard.nonce();

            let mut tx = TransactionRequest::default()
                .with_from(prepared.from)
                .with_value(prepared.value)
                .with_input(prepared.data.clone())
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority_fee)
                .with_gas_limit(prepared.gas_limit);
            if let Some(to) = prepared.to {
                tx = tx.with_to(to);
            }

            let typed_tx = match tx.build_typed_tx() {
                Ok(t) => t,
                Err(e) => {
                    warn!(from = %prepared.from, nonce, error = ?e, "failed to build typed tx");
                    nonce_guard.rollback();
                    let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("tx build failed".into())).await;
                    continue;
                }
            };

            let sig_hash = typed_tx.signature_hash();
            let signature = match signer.sign_hash_sync(&sig_hash) {
                Ok(sig) => sig,
                Err(e) => {
                    warn!(from = %prepared.from, nonce, error = %e, "failed to sign tx");
                    nonce_guard.rollback();
                    let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("signing failed".into())).await;
                    continue;
                }
            };

            let signed = typed_tx.into_signed(signature);
            let tx_hash = *signed.hash();
            let raw = Bytes::from(signed.encoded_2718());

            // Drop the nonce guard immediately after signing. The guard
            // holds the NonceManager mutex; keeping it alive until after the
            // batch RPC send would block concurrent batch tasks that share
            // the same sender, and cause a deadlock when batch_size >
            // sender_count (duplicate senders in the same batch).
            drop(nonce_guard);

            signed_txs.push(SignedTx {
                raw,
                tx_hash,
                from: prepared.from,
                nonce,
            });
        }

        if signed_txs.is_empty() {
            return 0;
        }

        // Phase 2: Send all signed transactions in a single JSON-RPC batch.
        let raw_list: Vec<Bytes> = signed_txs.iter().map(|s| s.raw.clone()).collect();
        let batch_results = match ctx.batch_rpc.send_raw_transactions(&raw_list).await {
            Ok(results) => results,
            Err(e) => {
                warn!(error = %e, count = signed_txs.len(), "batch RPC failed, falling back to individual sends");
                return Self::submit_individually(ctx, signed_txs).await;
            }
        };

        // Phase 3: Process batch results.
        let mut submitted = 0u64;
        let mut retry_txs: Vec<SignedTx> = Vec::new();

        for (signed, result) in signed_txs.into_iter().zip(batch_results) {
            match result {
                BatchSendResult::Success(hash) => {
                    // Server may return a different hash than we computed locally
                    // (shouldn't happen, but use server's hash for tracking).
                    let tracked_hash = if hash != signed.tx_hash {
                        debug!(
                            local = %signed.tx_hash,
                            server = %hash,
                            "tx hash mismatch, using server hash"
                        );
                        hash
                    } else {
                        signed.tx_hash
                    };
                    ctx.confirmer_handle.record_submitted(tracked_hash, signed.from).await;
                    let _ = ctx.submit_event_tx.send(SubmitEvent::Submitted(tracked_hash)).await;
                    debug!(tx_hash = %tracked_hash, from = %signed.from, nonce = signed.nonce, "tx submitted (batch)");
                    submitted += 1;
                }
                BatchSendResult::Error(ref msg) => {
                    let is_txpool_full =
                        msg.contains("txpool is full") || msg.contains("transaction pool is full");

                    if is_txpool_full {
                        retry_txs.push(signed);
                        continue;
                    }

                    if msg.contains("nonce too low") {
                        debug!(from = %signed.from, nonce = signed.nonce, "nonce too low, already confirmed on chain");
                        continue;
                    }

                    debug!(from = %signed.from, nonce = signed.nonce, error = %msg, "tx rejected in batch");
                    let _ = ctx.submit_event_tx.send(SubmitEvent::Failed(msg.clone())).await;
                }
            }
        }

        // Phase 4: Retry txpool-full rejections individually with backoff.
        if !retry_txs.is_empty() {
            debug!(count = retry_txs.len(), "retrying txpool-full rejections individually");
            submitted += Self::submit_individually(ctx, retry_txs).await;
        }

        submitted
    }

    async fn submit_individually(ctx: BatchSubmitCtx, signed_txs: Vec<SignedTx>) -> u64 {
        let mut submitted = 0u64;

        for signed in signed_txs {
            let Some(provider) = ctx.providers.get(&signed.from) else {
                warn!(from = %signed.from, "no cached provider for sender");
                let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("no provider".into())).await;
                continue;
            };

            let mut attempts = 0u32;
            let max_attempts = 3u32;
            let mut backoff = AdaptiveBackoff::default();

            loop {
                match provider.send_raw_transaction(&signed.raw).await {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        ctx.confirmer_handle.record_submitted(tx_hash, signed.from).await;
                        let _ = ctx.submit_event_tx.send(SubmitEvent::Submitted(tx_hash)).await;
                        debug!(tx_hash = %tx_hash, from = %signed.from, nonce = signed.nonce, "tx submitted (individual fallback)");
                        submitted += 1;
                        break;
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        attempts += 1;

                        let is_txpool_full = error_str.contains("txpool is full")
                            || error_str.contains("transaction pool is full");

                        if is_txpool_full && attempts < max_attempts {
                            backoff.record_error();
                            let delay = backoff.current();
                            debug!(
                                attempt = attempts,
                                backoff_ms = delay.as_millis(),
                                from = %signed.from,
                                nonce = signed.nonce,
                                "txpool full, retrying with backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }

                        if error_str.contains("nonce too low") {
                            debug!(from = %signed.from, nonce = signed.nonce, "nonce too low");
                            break;
                        }

                        debug!(from = %signed.from, nonce = signed.nonce, error = %error_str, "tx submission failed");
                        let _ = ctx.submit_event_tx.send(SubmitEvent::Failed(error_str)).await;
                        break;
                    }
                }
            }
        }

        submitted
    }

    /// Drains all test account balances back to the funder address.
    ///
    /// Each account sends its entire balance minus gas costs back to the funder.
    /// Transactions that fail (e.g. zero balance) are skipped with a warning.
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn drain_accounts(&self, funding_key: PrivateKeySigner) -> Result<U256> {
        let funder_address = funding_key.address();
        let client = self.client.clone();
        let rpc_url = self.config.rpc_http_url.clone();
        let chain_id = self.config.chain_id;

        let gas_price = client.get_gas_price().await?;
        let max_priority_fee = (gas_price / 10).max(1);
        // Ensure max_fee >= max_priority_fee (EIP-1559 requirement).
        let max_fee =
            gas_price.saturating_mul(2).max(max_priority_fee).min(self.config.max_gas_price);
        let drain_gas_limit = 21_000u128;
        // L1 data fee on Base can be significant (0.0001-0.001 ETH depending on L1 gas prices).
        // Use 0.001 ETH (1e15 wei) buffer to be safe. We may leave dust in accounts.
        let l1_fee_buffer = 1_000_000_000_000_000u128;
        let drain_gas_cost = U256::from(drain_gas_limit * max_fee + l1_fee_buffer);

        let total_accounts = self.accounts.len();
        let pb_drain = self.progress_bar(total_accounts as u64, "Draining accounts");

        // Each account has its own signer, so drains are fully independent.
        let account_data: Vec<_> =
            self.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();

        let drain_futs: Vec<_> = account_data
            .into_iter()
            .map(|(address, signer)| {
                let client = client.clone();
                let rpc_url = rpc_url.clone();
                async move {
                    let balance = client.get_pending_balance(address).await?;
                    if balance <= drain_gas_cost {
                        debug!(
                            address = %address,
                            balance = %balance,
                            "skipping drain, balance too low to cover gas"
                        );
                        return Ok::<_, BaselineError>(None);
                    }

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
                                amount = %send_amount,
                                tx_hash = %tx_hash,
                                "drain tx sent"
                            );
                            Ok(Some((tx_hash, address, send_amount)))
                        }
                        Err(e) => {
                            warn!(from = %address, error = %e, "drain tx failed, skipping");
                            Ok(None)
                        }
                    }
                }
            })
            .collect();

        let drain_results: Vec<_> = stream::iter(drain_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_drain.inc(1))
            .collect()
            .await;
        pb_drain.finish_and_clear();

        let mut pending_txs = Vec::new();
        let mut total_drained = U256::ZERO;
        for result in drain_results {
            if let Some((tx_hash, address, amount)) = result? {
                pending_txs.push((tx_hash, address));
                total_drained = total_drained.saturating_add(amount);
            }
        }

        if pending_txs.is_empty() {
            info!("no accounts to drain");
            return Ok(U256::ZERO);
        }

        let pb_confirm = self.progress_bar(pending_txs.len() as u64, "Confirming drain txs");
        info!(count = pending_txs.len(), total = %total_drained, "waiting for drain txs to confirm");

        if let Err(e) = Self::await_confirmations(&client, &mut pending_txs, &pb_confirm).await {
            warn!(error = %e, "some drain txs did not confirm within timeout");
        }
        pb_confirm.finish_and_clear();

        info!(total = %total_drained, "drain complete");
        Ok(total_drained)
    }

    fn progress_bar(&self, total: u64, prefix: &str) -> ProgressBar {
        if self.snapshot_tx.is_some() {
            return ProgressBar::hidden();
        }
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template("{prefix} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                .expect("valid template")
                .progress_chars("█▓░"),
        );
        pb.set_prefix(prefix.to_string());
        pb
    }

    async fn await_confirmations(
        client: &RpcClient,
        pending_txs: &mut Vec<(TxHash, Address)>,
        pb: &ProgressBar,
    ) -> Result<()> {
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();

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
                    Ok(Some(_)) => {
                        debug!(tx_hash = %tx_hash, address = %address, "tx confirmed");
                        pb.inc(1);
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
            return Err(BaselineError::Transaction(format!(
                "txs did not confirm within timeout: {unconfirmed:?}"
            )));
        }

        Ok(())
    }

    /// Checks account balances, stores the results for the live display, and
    /// logs a warning when any account is running low.
    async fn check_account_balances(&mut self) {
        let addresses: Vec<Address> = self.accounts.accounts().iter().map(|a| a.address).collect();

        let results =
            futures::future::join_all(addresses.iter().map(|&addr| self.client.get_balance(addr)))
                .await;

        let mut total = U256::ZERO;
        let mut min = U256::MAX;
        let mut below_threshold = 0usize;

        for (&address, result) in addresses.iter().zip(results) {
            match result {
                Ok(balance) => {
                    total = total.saturating_add(balance);
                    if balance < min {
                        min = balance;
                    }
                    if balance < U256::from(LOW_BALANCE_THRESHOLD) {
                        below_threshold += 1;
                    }
                }
                Err(e) => {
                    warn!(address = %address, error = %e, "failed to check account balance");
                }
            }
        }

        if min == U256::MAX {
            return;
        }

        self.last_total_eth = Some(format_ether(total));
        self.last_min_eth = Some(format_ether(min));
        self.last_funds_low = below_threshold > 0;

        if below_threshold > 0 {
            warn!(
                total_eth = %format_ether(total),
                min_eth = %format_ether(min),
                accounts_low = below_threshold,
                "account funds running low"
            );
        } else {
            info!(
                total_eth = %format_ether(total),
                min_eth = %format_ether(min),
                "account balances"
            );
        }
    }

    /// Signals the load test to stop gracefully.
    ///
    /// Only sets `stop_flag` — does **not** cancel WebSocket tasks or clean up
    /// resources. The caller must ensure [`run()`](Self::run) completes, which
    /// handles draining confirmations and cancelling background tasks.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    /// Returns a clone of the stop flag for external coordination.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    /// Returns the load configuration.
    pub const fn config(&self) -> &LoadConfig {
        &self.config
    }

    /// Attaches a live progress-bar display.
    ///
    /// When set and stdout is a TTY, the runner updates the indicatif bars
    /// every 500 ms instead of emitting 5-second progress log lines.
    pub fn set_display(&mut self, display: LoadTestDisplay) {
        self.display = Some(display);
    }

    /// Replaces the internal stop flag with an externally-owned one.
    ///
    /// Call this before [`run`] when the caller needs to share the flag across threads
    /// (e.g. a TUI view pre-creates the flag so it can stop the test without waiting
    /// for the runner to be fully initialised).
    pub fn replace_stop_flag(&mut self, flag: Arc<AtomicBool>) {
        self.stop_flag = flag;
    }

    /// Attaches a watch channel for streaming live [`DisplaySnapshot`] updates to a TUI view.
    ///
    /// When set, the runner publishes a snapshot every 500 ms during the run loop,
    /// regardless of whether a TTY display is also attached. The TUI view polls
    /// the corresponding [`watch::Receiver`] on each tick.
    pub fn set_snapshot_tx(&mut self, tx: watch::Sender<DisplaySnapshot>) {
        self.snapshot_tx = Some(tx);
    }
}

impl std::fmt::Debug for LoadRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadRunner")
            .field("config", &self.config)
            .field("accounts", &self.accounts.len())
            .field("providers_cached", &self.providers.len())
            .finish_non_exhaustive()
    }
}
