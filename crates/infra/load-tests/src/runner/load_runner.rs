use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U256, utils::format_ether};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::NonceManager;

/// Provider type for nonce management. Uses Ethereum network type because
/// `NonceManager` only calls `get_transaction_count`, which returns the same
/// response for both Ethereum and Base networks.
type NonceProvider = RootProvider<Ethereum>;
use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, warn};

use super::{
    AdaptiveBackoff, Confirmer, ConfirmerHandle, DisplaySnapshot, LoadConfig, LoadTestDisplay,
    RateLimiter, TxType,
};
use crate::{
    BaselineError, Result,
    config::WorkloadConfig,
    metrics::{MetricsCollector, MetricsSummary, TransactionMetrics},
    rpc::{RpcClient, WalletProvider, create_wallet_provider},
    workload::{
        AccountPool, CalldataPayload, Erc20Payload, PrecompilePayload, TransferPayload,
        WorkloadGenerator,
    },
};

struct PreparedTx {
    from: Address,
    to: Address,
    value: U256,
    data: Bytes,
    gas_limit: u64,
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
    nonce_managers: HashMap<Address, NonceManager<NonceProvider>>,
    providers: HashMap<Address, WalletProvider>,
    gas_price: u128,
    /// Optional live status display for TTY terminals.
    display: Option<LoadTestDisplay>,
    /// Last observed total ETH across all sender accounts (formatted).
    last_total_eth: Option<String>,
    /// Last observed minimum ETH in any single sender account (formatted).
    last_min_eth: Option<String>,
    /// Whether any account was below the low-balance threshold on the last check.
    last_funds_low: bool,
}

impl LoadRunner {
    /// Creates a new load runner with the given configuration.
    #[instrument(skip_all, fields(rpc_url = %config.rpc_url, chain_id = config.chain_id))]
    pub fn new(config: LoadConfig) -> Result<Self> {
        config.validate()?;

        let client = RpcClient::new(config.rpc_url.clone());

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

        let providers = Self::build_providers(&config.rpc_url, &accounts);

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
            nonce_managers: HashMap::new(),
            providers,
            gas_price: 0,
            display: None,
            last_total_eth: None,
            last_min_eth: None,
            last_funds_low: false,
        })
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
            let gas_estimate = match &tx_config.tx_type {
                TxType::Transfer => 21_000,
                TxType::Calldata { max_size, .. } => 21_000 + (*max_size as u64 * 16),
                TxType::Erc20 { .. } => 65_000,
                TxType::Precompile { iterations, .. } => 50_000 + 100_000 * (*iterations as u64),
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
        let mut accounts_to_fund = Vec::new();
        for account in self.accounts.accounts_mut() {
            let balance = self.client.get_balance(account.address).await?;
            account.balance = balance;
            let account_nonce = self.client.get_nonce(account.address).await?;
            account.nonce = account_nonce;

            if balance < amount_per_account {
                let deficit = amount_per_account.saturating_sub(balance);
                accounts_to_fund.push((account.address, deficit));
            } else {
                debug!(address = %account.address, balance = %balance, "account already funded");
            }
        }

        if accounts_to_fund.is_empty() {
            info!("all accounts already have sufficient balance, skipping funding");
            return Ok(());
        }

        let funder_address = funding_key.address();
        let wallet = EthereumWallet::from(funding_key);
        let funder_provider = create_wallet_provider(self.config.rpc_url.clone(), wallet);
        let mut nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        info!(
            from = %funder_address,
            amount = %amount_per_account,
            accounts_needing_funds = accounts_to_fund.len(),
            "funding accounts"
        );

        let gas_price = self.client.get_gas_price().await?;
        let max_fee = gas_price.saturating_mul(2).min(self.config.max_gas_price);
        let max_priority_fee = (gas_price / 10).max(1);

        let replacement_max_fee = max_fee.saturating_mul(3);
        let replacement_priority_fee = max_priority_fee.saturating_mul(3);

        let mut pending_txs = Vec::new();
        for (address, deficit) in &accounts_to_fund {
            let tx = TransactionRequest::default()
                .with_to(*address)
                .with_value(*deficit)
                .with_nonce(nonce)
                .with_chain_id(self.config.chain_id)
                .with_gas_limit(21_000)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(max_priority_fee);

            match funder_provider.send_transaction(tx).await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    debug!(to = %address, deficit = %deficit, nonce, tx_hash = %tx_hash, "funding tx sent");
                    pending_txs.push((tx_hash, *address));
                    nonce += 1;
                }
                Err(e) => {
                    let error_str = e.to_string();
                    if error_str.contains("already known") {
                        warn!(to = %address, nonce, "funding tx already in mempool, replacing with higher gas price");
                        let replacement = TransactionRequest::default()
                            .with_to(*address)
                            .with_value(*deficit)
                            .with_nonce(nonce)
                            .with_chain_id(self.config.chain_id)
                            .with_gas_limit(21_000)
                            .with_max_fee_per_gas(replacement_max_fee)
                            .with_max_priority_fee_per_gas(replacement_priority_fee);

                        match funder_provider.send_transaction(replacement).await {
                            Ok(pending) => {
                                let tx_hash = *pending.tx_hash();
                                info!(to = %address, nonce, tx_hash = %tx_hash, "replacement funding tx sent");
                                pending_txs.push((tx_hash, *address));
                            }
                            Err(replace_err) => {
                                warn!(to = %address, nonce, error = %replace_err, "replacement tx also failed, proceeding");
                            }
                        }
                        nonce += 1;
                        continue;
                    }
                    error!(to = %address, error = %e, "failed to fund account");
                    return Err(BaselineError::Transaction(format!(
                        "failed to fund {address}: {e}",
                    )));
                }
            }
        }

        info!(count = pending_txs.len(), "waiting for funding txs to confirm");
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();

        while !pending_txs.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for (tx_hash, address) in pending_txs {
                match self.client.get_transaction_receipt(tx_hash).await {
                    Ok(Some(_)) => {
                        debug!(tx_hash = %tx_hash, address = %address, "funding tx confirmed");
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
            pending_txs = still_pending;
        }

        if !pending_txs.is_empty() {
            let unconfirmed: Vec<_> = pending_txs.iter().map(|(_, addr)| addr).collect();
            return Err(BaselineError::Transaction(format!(
                "funding txs did not confirm within timeout: {unconfirmed:?}"
            )));
        }

        for account in self.accounts.accounts_mut() {
            let balance = self.client.get_balance(account.address).await?;
            account.balance = balance;
            let account_nonce = self.client.get_nonce(account.address).await?;
            account.nonce = account_nonce;

            let provider = NonceProvider::new_http(self.config.rpc_url.clone());
            let nonce_manager = NonceManager::new(provider, account.address, NONCE_RPC_TIMEOUT);
            self.nonce_managers.insert(account.address, nonce_manager);

            debug!(address = %account.address, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        info!(funded = accounts_to_fund.len(), "funding complete");
        Ok(())
    }

    /// Runs the load test and returns metrics summary.
    #[instrument(skip(self), fields(target_gps = self.config.target_gps, continuous = self.config.duration.is_none(), duration = ?self.config.duration))]
    pub async fn run(&mut self) -> Result<MetricsSummary> {
        self.collector.reset();
        self.collector.start();
        self.stop_flag.store(false, Ordering::SeqCst);

        self.gas_price = self.client.get_gas_price().await?;
        info!(gas_price = self.gas_price, "fetched current gas price");

        for account in self.accounts.accounts() {
            if !self.nonce_managers.contains_key(&account.address) {
                let provider = NonceProvider::new_http(self.config.rpc_url.clone());
                let nonce_manager = NonceManager::new(provider, account.address, NONCE_RPC_TIMEOUT);
                self.nonce_managers.insert(account.address, nonce_manager);
            }
        }

        for (address, nonce_manager) in &self.nonce_managers {
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
        let (metrics_tx, mut metrics_rx) =
            mpsc::channel::<TransactionMetrics>(METRICS_CHANNEL_BUFFER);

        let sender_addresses: Vec<_> = self.accounts.accounts().iter().map(|a| a.address).collect();
        let mut confirmer =
            Confirmer::new(&sender_addresses, metrics_tx, Arc::clone(&self.stop_flag));
        let confirmer_handle = confirmer.handle();
        let confirmer_handle_for_run = confirmer_handle.clone();

        let confirmer_client = RpcClient::new(self.config.rpc_url.clone());
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
        let mut batch_start = Instant::now();
        let mut backoff = AdaptiveBackoff::default();

        let mut consecutive_at_limit = 0usize;
        let mut last_gas_price_refresh = Instant::now();
        let mut last_rate_limiter_update = Instant::now();
        let mut last_progress_report = Instant::now();
        let mut last_balance_check = Instant::now();
        const GAS_PRICE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
        const RATE_LIMITER_UPDATE_INTERVAL: Duration = Duration::from_secs(10);
        const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
        const DISPLAY_RENDER_INTERVAL: Duration = Duration::from_millis(500);
        const BALANCE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

        let use_live_display = self.display.as_ref().is_some_and(|d| d.is_active());

        self.check_account_balances().await;

        while self.config.duration.is_none_or(|d| start.elapsed() < d)
            && !self.stop_flag.load(Ordering::SeqCst)
        {
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
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    consecutive_at_limit = 0;
                    rate_limiter.reset_tick();
                }
                continue;
            }

            consecutive_at_limit = 0;

            rate_limiter.tick().await;

            let from = account.address;
            let to_idx = (current_account_idx + 1) % account_count;
            let to = self.accounts.accounts()[to_idx].address;

            let tx_request = self.generator.generate_payload(from, to)?;

            let to_addr = tx_request.to.and_then(|kind| kind.to().copied()).unwrap_or(to);
            let value = tx_request.value.unwrap_or(U256::ZERO);
            let data = tx_request.input.input().cloned().unwrap_or_default();
            let gas_limit = tx_request.gas.unwrap_or(21_000);

            pending_batch.push(PreparedTx { from, to: to_addr, value, data, gas_limit });

            current_account_idx = (current_account_idx + 1) % account_count;

            let should_flush =
                pending_batch.len() >= batch_size || batch_start.elapsed() >= batch_timeout;

            if should_flush && !pending_batch.is_empty() {
                let batch = std::mem::replace(&mut pending_batch, Vec::with_capacity(batch_size));
                batch_start = Instant::now();

                let submitted = self.submit_batch(batch, &confirmer_handle, &mut backoff).await;

                debug!(submitted, "batch submitted");
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

            if use_live_display {
                if last_progress_report.elapsed() >= DISPLAY_RENDER_INTERVAL {
                    let (p50, p99) = self.collector.rolling_p50_p99();
                    let snap = DisplaySnapshot {
                        elapsed: start.elapsed(),
                        duration: self.config.duration,
                        submitted: self.collector.submitted_count(),
                        confirmed: self.collector.confirmed_count(),
                        failed: self.collector.failed_count(),
                        in_flight: confirmer_handle.total_in_flight(),
                        senders_blocked: confirmer_handle
                            .senders_at_limit(max_in_flight_per_sender),
                        total_senders: account_count,
                        rolling_tps: self.collector.rolling_tps(),
                        rolling_gps: self.collector.rolling_gps(),
                        p50_latency: p50,
                        p99_latency: p99,
                        gas_price_gwei: self.gas_price as f64 / 1e9,
                        total_eth: self.last_total_eth.clone(),
                        min_eth: self.last_min_eth.clone(),
                        funds_low: self.last_funds_low,
                    };
                    self.display.as_ref().unwrap().update(&snap);
                    last_progress_report = Instant::now();
                }
            } else if last_progress_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
                let elapsed_secs = start.elapsed().as_secs();
                let submitted = self.collector.submitted_count();
                let confirmed = self.collector.confirmed_count();
                let failed = self.collector.failed_count();
                let in_flight = confirmer_handle.total_in_flight();
                let senders_blocked = confirmer_handle.senders_at_limit(max_in_flight_per_sender);
                info!(
                    elapsed_secs,
                    submitted,
                    confirmed,
                    failed,
                    in_flight,
                    senders_blocked,
                    gas_price = self.gas_price,
                    "progress"
                );
                last_progress_report = Instant::now();
            }
        }

        if !pending_batch.is_empty() {
            let submitted = self.submit_batch(pending_batch, &confirmer_handle, &mut backoff).await;

            debug!(submitted, "final batch submitted");
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

        confirmer_task.abort();

        let confirmed = self.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        Ok(self.collector.summarize())
    }

    async fn submit_batch(
        &mut self,
        batch: Vec<PreparedTx>,
        confirmer_handle: &ConfirmerHandle,
        backoff: &mut AdaptiveBackoff,
    ) -> u64 {
        let mut submitted_count = 0u64;
        let chain_id = self.config.chain_id;

        for prepared in batch {
            let Some(provider) = self.providers.get(&prepared.from) else {
                warn!(from = %prepared.from, "no cached provider for sender");
                continue;
            };

            let Some(nonce_manager) = self.nonce_managers.get(&prepared.from) else {
                warn!(from = %prepared.from, "no nonce manager for sender");
                continue;
            };

            let nonce_guard = match nonce_manager.next_nonce().await {
                Ok(guard) => guard,
                Err(e) => {
                    warn!(from = %prepared.from, error = %e, "failed to acquire nonce");
                    continue;
                }
            };
            let nonce = nonce_guard.nonce();

            let max_fee = self.gas_price.saturating_mul(2).min(self.config.max_gas_price);
            let tx = TransactionRequest::default()
                .with_from(prepared.from)
                .with_to(prepared.to)
                .with_value(prepared.value)
                .with_input(prepared.data)
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas((self.gas_price / 10).max(1))
                .with_gas_limit(prepared.gas_limit);

            let mut attempts = 0;
            let max_attempts = 3;
            let mut should_rollback = false;

            loop {
                match provider.send_transaction(tx.clone()).await {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        confirmer_handle.record_submitted(tx_hash, prepared.from).await;
                        self.collector.record_submitted(tx_hash);
                        submitted_count += 1;
                        backoff.record_success();

                        debug!(
                            tx_hash = %tx_hash,
                            from = %prepared.from,
                            nonce,
                            "tx submitted"
                        );

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
                                from = %prepared.from,
                                nonce,
                                "txpool full, retrying with adaptive backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }

                        if error_str.contains("nonce too low") {
                            debug!(
                                from = %prepared.from,
                                nonce,
                                "nonce too low, already confirmed on chain"
                            );
                            break;
                        }

                        debug!(
                            from = %prepared.from,
                            nonce,
                            error = %error_str,
                            "tx submission failed"
                        );
                        self.collector.record_failed(alloy_primitives::TxHash::ZERO, &error_str);
                        backoff.record_error();
                        should_rollback = true;
                        break;
                    }
                }
            }

            if should_rollback {
                nonce_guard.rollback();
            }
        }

        submitted_count
    }

    /// Drains all test account balances back to the funder address.
    ///
    /// Each account sends its entire balance minus gas costs back to the funder.
    /// Transactions that fail (e.g. zero balance) are skipped with a warning.
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn drain_accounts(&self, funding_key: PrivateKeySigner) -> Result<U256> {
        let funder_address = funding_key.address();
        let gas_price = self.client.get_gas_price().await?;
        let max_fee = gas_price.saturating_mul(2).min(self.config.max_gas_price);
        let max_priority_fee = (gas_price / 10).max(1);
        let drain_gas_limit = 21_000u128;
        // L1 data fee on OP Stack can be significant (0.0001-0.001 ETH depending on L1 gas prices).
        // Use 0.001 ETH (1e15 wei) buffer to be safe. We may leave dust in accounts.
        let l1_fee_buffer = 1_000_000_000_000_000u128;
        let drain_gas_cost = U256::from(drain_gas_limit * max_fee + l1_fee_buffer);

        let mut pending_txs = Vec::new();
        let mut total_drained = U256::ZERO;

        for account in self.accounts.accounts() {
            let balance = self.client.get_pending_balance(account.address).await?;
            if balance <= drain_gas_cost {
                debug!(
                    address = %account.address,
                    balance = %balance,
                    "skipping drain, balance too low to cover gas"
                );
                continue;
            }

            let send_amount = balance.saturating_sub(drain_gas_cost);
            let wallet = EthereumWallet::from(account.signer.clone());
            let provider = create_wallet_provider(self.config.rpc_url.clone(), wallet);
            let nonce = provider
                .get_transaction_count(account.address)
                .pending()
                .await
                .map_err(|e| BaselineError::Rpc(e.to_string()))?;

            let tx = TransactionRequest::default()
                .with_to(funder_address)
                .with_value(send_amount)
                .with_nonce(nonce)
                .with_chain_id(self.config.chain_id)
                .with_gas_limit(drain_gas_limit as u64)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(max_priority_fee);

            match provider.send_transaction(tx).await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    debug!(
                        from = %account.address,
                        amount = %send_amount,
                        tx_hash = %tx_hash,
                        "drain tx sent"
                    );
                    pending_txs.push((tx_hash, account.address));
                    total_drained = total_drained.saturating_add(send_amount);
                }
                Err(e) => {
                    warn!(from = %account.address, error = %e, "drain tx failed, skipping");
                }
            }
        }

        if pending_txs.is_empty() {
            info!("no accounts to drain");
            return Ok(U256::ZERO);
        }

        info!(count = pending_txs.len(), total = %total_drained, "waiting for drain txs to confirm");
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();

        while !pending_txs.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for (tx_hash, address) in pending_txs {
                match self.client.get_transaction_receipt(tx_hash).await {
                    Ok(Some(_)) => {
                        debug!(tx_hash = %tx_hash, from = %address, "drain tx confirmed");
                    }
                    Ok(None) => {
                        still_pending.push((tx_hash, address));
                    }
                    Err(e) => {
                        warn!(tx_hash = %tx_hash, error = %e, "failed to get drain receipt");
                        still_pending.push((tx_hash, address));
                    }
                }
            }
            pending_txs = still_pending;
        }

        if !pending_txs.is_empty() {
            let unconfirmed: Vec<_> = pending_txs.iter().map(|(_, addr)| addr).collect();
            warn!(accounts = ?unconfirmed, "some drain txs did not confirm within timeout");
        }

        info!(total = %total_drained, "drain complete");
        Ok(total_drained)
    }

    /// Checks account balances, stores the results for the live display, and
    /// logs a warning when any account is running low.
    async fn check_account_balances(&mut self) {
        let mut total = U256::ZERO;
        let mut min = U256::MAX;
        let mut below_threshold = 0usize;

        for account in self.accounts.accounts() {
            match self.client.get_balance(account.address).await {
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
                    warn!(address = %account.address, error = %e, "failed to check account balance");
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

    /// Signals the load test to stop.
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
