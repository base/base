use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest as AlloyTxRequest;
use alloy_signer_local::PrivateKeySigner;
use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, warn};

use super::{
    AdaptiveBackoff, Confirmer, ConfirmerHandle, LoadConfig, NonceTracker, RateLimiter, TxType,
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
    nonce: u64,
    gas_limit: u64,
}

/// Executes load tests by generating and submitting transactions at a target rate.
pub struct LoadRunner {
    config: LoadConfig,
    client: RpcClient,
    accounts: AccountPool,
    generator: WorkloadGenerator,
    collector: MetricsCollector,
    stop_flag: Arc<AtomicBool>,
    nonce_tracker: NonceTracker,
    providers: HashMap<Address, WalletProvider>,
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
        let generator = Self::create_generator(workload_config, &config, &accounts)?;

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
            nonce_tracker: NonceTracker::new(),
            providers,
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
        accounts: &AccountPool,
    ) -> Result<WorkloadGenerator> {
        let accounts_clone = AccountPool::new(workload_config.seed.unwrap_or(0), accounts.len())?;
        let mut generator = WorkloadGenerator::new(workload_config, accounts_clone);

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
                TxType::Calldata { max_size } => {
                    generator = generator.with_payload(CalldataPayload::new(*max_size), weight_pct);
                }
                TxType::Erc20 { contract } => {
                    generator = generator.with_payload(
                        Erc20Payload::new(*contract, U256::from(1000), U256::from(10000)),
                        weight_pct,
                    );
                }
                TxType::Precompile { target } => {
                    generator = generator.with_payload(PrecompilePayload::new(*target), weight_pct);
                }
            }
        }

        Ok(generator)
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
                accounts_to_fund.push(account.address);
            } else {
                debug!(address = %account.address, balance = %balance, "account already funded");
            }
        }

        if accounts_to_fund.is_empty() {
            info!("all accounts already have sufficient balance, skipping funding");
            return Ok(());
        }

        let wallet = EthereumWallet::from(funding_key.clone());
        let funder_provider = create_wallet_provider(self.config.rpc_url.clone(), wallet);

        let funder_address = funding_key.address();
        let mut nonce = funder_provider
            .get_transaction_count(funder_address)
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        info!(
            from = %funder_address,
            amount = %amount_per_account,
            accounts_needing_funds = accounts_to_fund.len(),
            "funding accounts"
        );

        for address in &accounts_to_fund {
            let tx = AlloyTxRequest::default()
                .with_to(*address)
                .with_value(amount_per_account)
                .with_nonce(nonce)
                .with_chain_id(self.config.chain_id)
                .with_gas_limit(21_000);

            match funder_provider.send_transaction(tx).await {
                Ok(pending) => {
                    debug!(to = %address, nonce, tx_hash = %pending.tx_hash(), "funding tx sent");
                    nonce += 1;
                }
                Err(e) => {
                    error!(to = %address, error = %e, "failed to fund account");
                    return Err(BaselineError::Transaction(format!(
                        "failed to fund {address}: {e}",
                    )));
                }
            }
        }

        info!("waiting for funding txs to confirm");
        tokio::time::sleep(Duration::from_secs(5)).await;

        for account in self.accounts.accounts_mut() {
            let balance = self.client.get_balance(account.address).await?;
            account.balance = balance;
            let account_nonce = self.client.get_nonce(account.address).await?;
            account.nonce = account_nonce;

            self.nonce_tracker.init_account(account.address, account_nonce);

            debug!(address = %account.address, balance = %balance, nonce = account_nonce, "account state updated");
        }

        info!(funded = accounts_to_fund.len(), "funding complete");
        Ok(())
    }

    /// Runs the load test and returns metrics summary.
    #[instrument(skip(self), fields(tps = self.config.tps, duration = ?self.config.duration))]
    pub async fn run(&mut self) -> Result<MetricsSummary> {
        self.collector.reset();
        self.collector.start();
        self.stop_flag.store(false, Ordering::SeqCst);

        for account in self.accounts.accounts() {
            if self.nonce_tracker.pending_count(&account.address) == 0 {
                self.nonce_tracker.init_account(account.address, account.nonce);
            }
        }

        let (metrics_tx, mut metrics_rx) = mpsc::unbounded_channel::<TransactionMetrics>();

        let sender_addresses: Vec<_> = self.accounts.accounts().iter().map(|a| a.address).collect();
        let confirmer = Confirmer::new(
            self.config.rpc_url.clone(),
            &sender_addresses,
            metrics_tx,
            Arc::clone(&self.stop_flag),
        );
        let (confirmer_handle, pending_rx) = confirmer.handle();

        let confirmer_task = tokio::spawn(confirmer.run(pending_rx));

        let max_in_flight_per_sender = self.config.max_in_flight_per_sender;

        let mut rate_limiter = RateLimiter::new(self.config.tps);
        let start = Instant::now();
        let mut current_account_idx = 0usize;
        let account_count = self.accounts.len();

        let batch_size = self.config.batch_size;
        let batch_timeout = self.config.batch_timeout;

        info!(
            max_in_flight_per_sender,
            batch_size,
            batch_timeout_ms = batch_timeout.as_millis(),
            "starting load test with per-sender in-flight limiting"
        );

        let mut pending_batch: Vec<PreparedTx> = Vec::with_capacity(batch_size);
        let mut batch_start = Instant::now();
        let mut backoff = AdaptiveBackoff::default();

        while start.elapsed() < self.config.duration && !self.stop_flag.load(Ordering::SeqCst) {
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

                if self.all_senders_at_limit(&confirmer_handle, max_in_flight_per_sender) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                continue;
            }

            rate_limiter.tick().await;

            let to_idx = (current_account_idx + 1) % account_count;
            let to = self.accounts.accounts()[to_idx].address;

            let tx_request = self.generator.generate_batch(1)?;
            let tx_request = &tx_request[0];

            let from = account.address;

            let Some(nonce) = self.nonce_tracker.allocate(&from) else {
                warn!(address = %from, "failed to allocate nonce");
                current_account_idx = (current_account_idx + 1) % account_count;
                continue;
            };

            pending_batch.push(PreparedTx {
                from,
                to,
                value: tx_request.value,
                data: tx_request.data.clone(),
                nonce,
                gas_limit: tx_request.gas_limit.unwrap_or(21_000),
            });

            current_account_idx = (current_account_idx + 1) % account_count;

            let should_flush =
                pending_batch.len() >= batch_size || batch_start.elapsed() >= batch_timeout;

            if should_flush && !pending_batch.is_empty() {
                let batch = std::mem::replace(&mut pending_batch, Vec::with_capacity(batch_size));
                batch_start = Instant::now();

                let submitted = self.submit_batch(batch, &confirmer_handle, &mut backoff).await;

                debug!(submitted, "batch submitted");
            }
        }

        if !pending_batch.is_empty() {
            let submitted = self.submit_batch(pending_batch, &confirmer_handle, &mut backoff).await;

            debug!(submitted, "final batch submitted");
        }

        self.stop_flag.store(true, Ordering::SeqCst);

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

        let confirmed = self.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        Ok(self.collector.summarize())
    }

    fn all_senders_at_limit(&self, handle: &ConfirmerHandle, max: u64) -> bool {
        for account in self.accounts.accounts() {
            if handle.in_flight_for(&account.address) < max {
                return false;
            }
        }
        true
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

            let tx = AlloyTxRequest::default()
                .with_from(prepared.from)
                .with_to(prepared.to)
                .with_value(prepared.value)
                .with_input(prepared.data.clone())
                .with_nonce(prepared.nonce)
                .with_chain_id(chain_id)
                .with_max_fee_per_gas(1_000_000_000u128)
                .with_max_priority_fee_per_gas(0)
                .with_gas_limit(prepared.gas_limit);

            let mut attempts = 0;
            let max_attempts = 3;

            loop {
                match provider.send_transaction(tx.clone()).await {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        confirmer_handle.record_submitted(tx_hash, prepared.from);
                        self.collector.record_submitted(tx_hash);
                        submitted_count += 1;
                        backoff.record_success();

                        debug!(
                            tx_hash = %tx_hash,
                            from = %prepared.from,
                            nonce = prepared.nonce,
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
                                nonce = prepared.nonce,
                                "txpool full, retrying with adaptive backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }

                        if error_str.contains("nonce too low") {
                            debug!(
                                from = %prepared.from,
                                nonce = prepared.nonce,
                                "nonce too low, marking as confirmed"
                            );
                            self.nonce_tracker.confirm(&prepared.from, prepared.nonce);
                            break;
                        }

                        warn!(
                            from = %prepared.from,
                            nonce = prepared.nonce,
                            error = %error_str,
                            "tx submission failed"
                        );
                        self.nonce_tracker.fail(&prepared.from, prepared.nonce);
                        self.collector.record_failed(alloy_primitives::TxHash::ZERO, &error_str);
                        backoff.record_error();
                        break;
                    }
                }
            }
        }

        submitted_count
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
