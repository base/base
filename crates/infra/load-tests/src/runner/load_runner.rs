use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, TxHash, U256, utils::format_ether};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use base_common_network::Base;
use base_tx_manager::NonceManager;
use futures::{StreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use revm::precompile::PrecompileId;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

/// Maximum number of concurrent RPC requests during funding/draining operations.
const FUNDING_CONCURRENCY: usize = 32;

/// Maximum number of funding TXs to send before waiting for confirmation.
/// Kept below typical per-sender txpool limits (e.g. reth default is 16) to
/// avoid "txpool is full" rejections when all TXs originate from one funder.
const FUNDING_BATCH_SIZE: usize = 16;

use super::{
    BlockWatcher, DisplaySnapshot, FlashblockWatcher, LoadConfig, LoadTestDisplay, PreparedBatch,
    PreparedTransaction, QueuedSubmitFailures, RateLimiter, ResultsTracker, SubmissionPipeline,
    SubmitEvent, TxType,
};
use crate::{
    BaselineError, Result,
    config::{OsakaTarget, WorkloadConfig},
    metrics::{ConfigSummary, MetricsCollector, MetricsSummary},
    rpc::{
        BatchRpcClient, QueryProvider, RpcProviders, RpcResultExt, TxpoolAdminClient,
        create_wallet_provider,
    },
    workload::{
        AccountPool, AerodromeClPayload, CalldataPayload, Erc20Payload, OsakaPayload,
        PrecompilePayload, TransferPayload, UniswapV3Payload, WorkloadGenerator,
    },
};

const NONCE_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const SUBMIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const SUBMIT_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const PENDING_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(60);
const CONFIRMATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(65);
const TXPOOL_CLEAR_CONCURRENCY: usize = 64;

/// Executes load tests by generating and submitting transactions at a target rate.
pub struct LoadRunner {
    config: LoadConfig,
    config_summary: Option<ConfigSummary>,
    client: QueryProvider,
    accounts: AccountPool,
    generator: WorkloadGenerator,
    collector: MetricsCollector,
    stop_flag: Arc<AtomicBool>,
    cancel_token: CancellationToken,
    nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    submission_batch_rpcs: Arc<Vec<BatchRpcClient>>,
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
    #[instrument(
        skip_all,
        fields(
            primary_submission_rpc = %config.primary_submission_rpc(),
            submission_rpc_count = config.transaction_submission_rpcs.len(),
            query_rpc = %config.query_rpc,
            chain_id = config.chain_id,
        )
    )]
    pub fn new(config: LoadConfig) -> Result<Self> {
        config.validate()?;

        let client = RpcProviders::query(config.query_rpc.clone())?;

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

        let signers = Arc::new(Self::build_signers(&accounts));
        let submission_batch_rpcs = Arc::new(
            config
                .transaction_submission_rpcs
                .iter()
                .cloned()
                .map(BatchRpcClient::new)
                .collect::<Vec<_>>(),
        );
        let sender_addresses = accounts.accounts().iter().map(|a| a.address.to_string()).collect();

        let workload_config = WorkloadConfig::new("load-test").with_seed(config.seed);
        let generator = Self::create_generator(workload_config, &config)?;

        info!(
            account_count = config.account_count,
            signers_cached = signers.len(),
            submission_rpc_count = submission_batch_rpcs.len(),
            "load runner created"
        );

        Ok(Self {
            config,
            config_summary: None,
            client,
            accounts,
            generator,
            collector: MetricsCollector::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
            nonce_managers: Arc::new(HashMap::new()),
            signers,
            submission_batch_rpcs,
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

    /// Sets the config summary for inclusion in JSON output.
    pub fn set_config_summary(&mut self, summary: ConfigSummary) {
        self.config_summary = Some(summary);
    }

    /// Returns the number of configured txpool nodes to clear before test startup.
    pub const fn txpool_node_count(&self) -> usize {
        self.config.txpool_nodes.len()
    }

    fn build_signers(accounts: &AccountPool) -> HashMap<Address, PrivateKeySigner> {
        accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect()
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
                TxType::UniswapV3 { router, token_in, token_out, fee, min_amount, max_amount } => {
                    generator = generator.with_payload(
                        UniswapV3Payload::new(
                            *router,
                            *token_in,
                            *token_out,
                            *fee,
                            *min_amount,
                            *max_amount,
                        ),
                        weight_pct,
                    );
                }
                TxType::AerodromeCl {
                    router,
                    token_in,
                    token_out,
                    tick_spacing,
                    min_amount,
                    max_amount,
                } => {
                    generator = generator.with_payload(
                        AerodromeClPayload::new(
                            *router,
                            *token_in,
                            *token_out,
                            *tick_spacing,
                            *min_amount,
                            *max_amount,
                        ),
                        weight_pct,
                    );
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
                TxType::UniswapV3 { .. } | TxType::AerodromeCl { .. } => 250_000,
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
        let primary_submission_rpc = self.config.primary_submission_rpc().clone();
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
                    let balance = client.get_balance(addr).await.rpc("get balance")?;
                    let nonce =
                        client.get_transaction_count(addr).await.rpc("get transaction count")?;
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
        let funder_provider =
            Arc::new(create_wallet_provider(primary_submission_rpc.clone(), wallet));

        let gas_price = client.get_gas_price().await.rpc("get gas price")?;
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

        let funder_balance = client.get_balance(funder_address).await.rpc("get balance")?;

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
            .rpc("get pending transaction count")?;

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
            let mut batch_pending: Vec<Address> = Vec::with_capacity(batch.len());
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

            let mut nonce_refresh_needed: Vec<(Address, U256)> = Vec::new();

            while let Some((result, address, deficit, nonce)) = send_stream.next().await {
                match result {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        debug!(to = %address, deficit = %deficit, nonce, tx_hash = %tx_hash, "funding tx sent");
                        batch_pending.push(address);
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        if error_str.contains("already known") {
                            retries.push((address, deficit, nonce));
                        } else if error_str.contains("nonce too low") {
                            info!(to = %address, nonce, "nonce too low, will refresh and retry");
                            nonce_refresh_needed.push((address, deficit));
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
                            batch_pending.push(address);
                        }
                        Err(replace_err) => {
                            warn!(to = %address, nonce, error = %replace_err, "replacement tx also failed, proceeding");
                        }
                    }
                }
            }

            Self::await_balances(&client, &mut batch_pending, amount_per_account, &pb_fund).await?;

            if !nonce_refresh_needed.is_empty() {
                let fresh_nonce = funder_provider
                    .get_transaction_count(funder_address)
                    .pending()
                    .await
                    .rpc("get pending transaction count")?;

                info!(
                    count = nonce_refresh_needed.len(),
                    fresh_nonce, "retrying funding txs with refreshed nonce"
                );

                let nonce_retry_futs =
                    nonce_refresh_needed.into_iter().enumerate().map(|(i, (address, deficit))| {
                        let provider = Arc::clone(&funder_provider);
                        let retry_nonce = fresh_nonce + i as u64;
                        async move {
                            let tx = TransactionRequest::default()
                                .with_to(address)
                                .with_value(deficit)
                                .with_nonce(retry_nonce)
                                .with_chain_id(chain_id)
                                .with_gas_limit(21_000)
                                .with_max_fee_per_gas(max_fee)
                                .with_max_priority_fee_per_gas(max_priority_fee);
                            let result = provider.send_transaction(tx).await;
                            (result, address, retry_nonce)
                        }
                    });

                let mut nonce_retry_stream =
                    stream::iter(nonce_retry_futs).buffered(FUNDING_BATCH_SIZE);

                let mut nonce_retry_pending: Vec<Address> = Vec::new();
                while let Some((result, address, retry_nonce)) = nonce_retry_stream.next().await {
                    match result {
                        Ok(pending) => {
                            let tx_hash = *pending.tx_hash();
                            info!(to = %address, nonce = retry_nonce, tx_hash = %tx_hash, "nonce-refreshed funding tx sent");
                            nonce_retry_pending.push(address);
                        }
                        Err(retry_err) => {
                            warn!(to = %address, nonce = retry_nonce, error = %retry_err, "nonce-refreshed retry also failed, proceeding");
                        }
                    }
                }

                Self::await_balances(
                    &client,
                    &mut nonce_retry_pending,
                    amount_per_account,
                    &pb_fund,
                )
                .await?;
            }
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
                    let balance = client.get_balance(addr).await.rpc("get balance")?;
                    let nonce =
                        client.get_transaction_count(addr).await.rpc("get transaction count")?;
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

            let provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());
            let nonce_manager =
                NonceManager::new(provider, addr, NONCE_RPC_TIMEOUT).with_pending_tag();
            Arc::make_mut(&mut self.nonce_managers).insert(addr, nonce_manager);

            debug!(address = %addr, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        info!(funded = accounts_to_fund.len(), "funding complete");
        Ok(())
    }

    /// Collects unique token addresses from configured swap transaction types.
    pub fn collect_swap_tokens(&self) -> Vec<Address> {
        let mut tokens = std::collections::HashSet::new();
        for tx_config in &self.config.transactions {
            match &tx_config.tx_type {
                TxType::UniswapV3 { token_in, token_out, .. }
                | TxType::AerodromeCl { token_in, token_out, .. } => {
                    tokens.insert(*token_in);
                    tokens.insert(*token_out);
                }
                TxType::Transfer
                | TxType::Calldata { .. }
                | TxType::Erc20 { .. }
                | TxType::Precompile { .. }
                | TxType::Osaka { .. } => {}
            }
        }
        tokens.into_iter().collect()
    }

    /// Clears pending transactions from all configured txpool nodes for every test sender.
    #[instrument(skip(self), fields(nodes = self.config.txpool_nodes.len(), accounts = self.accounts.len()))]
    pub async fn clear_txpools(&self) -> Result<u64> {
        if self.config.txpool_nodes.is_empty() {
            return Ok(0);
        }

        info!(
            nodes = self.config.txpool_nodes.len(),
            accounts = self.accounts.len(),
            "clearing txpool sender transactions"
        );

        let clients: Vec<_> = self
            .config
            .txpool_nodes
            .iter()
            .cloned()
            .map(|node| {
                let client = TxpoolAdminClient::new(node.clone())?;
                Ok::<_, BaselineError>((node, client))
            })
            .collect::<Result<_>>()?;
        let addresses: Vec<_> =
            self.accounts.accounts().iter().map(|account| account.address).collect();
        let requests: Vec<_> = clients
            .iter()
            .flat_map(|(node, client)| {
                addresses
                    .iter()
                    .copied()
                    .map(move |address| (node.clone(), client.clone(), address))
            })
            .collect();

        let clear_results: Vec<_> =
            stream::iter(requests.into_iter().map(|(node, client, address)| async move {
                let removed = client.drop_sender_transactions(address).await.map_err(|e| {
                    BaselineError::Rpc(format!(
                        "failed to clear txpool node {node} for sender {address}: {e}"
                    ))
                })?;
                Ok::<_, BaselineError>((node, removed.len() as u64))
            }))
            .buffer_unordered(TXPOOL_CLEAR_CONCURRENCY)
            .collect()
            .await;

        let mut removed_by_node: HashMap<url::Url, u64> = HashMap::new();
        for result in clear_results {
            let (node, removed) = result?;
            removed_by_node
                .entry(node)
                .and_modify(|total| *total = total.saturating_add(removed))
                .or_insert(removed);
        }

        let mut removed_total = 0u64;
        for node in &self.config.txpool_nodes {
            let removed_for_node = removed_by_node.get(node).copied().unwrap_or(0);
            removed_total = removed_total.saturating_add(removed_for_node);
            info!(
                node = %node,
                removed = removed_for_node,
                "cleared txpool sender transactions from node"
            );
        }

        info!(removed = removed_total, "txpool clearing complete");
        Ok(removed_total)
    }

    /// Mints swap tokens to all sender accounts.
    ///
    /// Scans the configured transaction types for token addresses, then mints
    /// `amount_per_token` of each token to every sender that has insufficient balance.
    /// Skips accounts that already have enough tokens. Requires tokens that expose
    /// a public `mint(address,uint256)` function (e.g., `FreeTransferERC20`).
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn setup_swap_tokens(
        &self,
        funding_key: PrivateKeySigner,
        amount_per_token: U256,
    ) -> Result<()> {
        let tokens = self.collect_swap_tokens();
        if tokens.is_empty() {
            debug!("no swap tokens configured, skipping token setup");
            return Ok(());
        }

        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();
        let token_count = tokens.len();
        let total_pairs = token_count * sender_addresses.len();

        // Phase 1: Check existing token balances for all (token, sender) pairs.
        let pb_check = self.progress_bar(total_pairs as u64, "Checking token balances");
        let client = &self.client;

        let balance_futs: Vec<_> = tokens
            .iter()
            .flat_map(|&token| {
                sender_addresses.iter().map(move |&sender| {
                    let client = client.clone();
                    let call_data = Self::encode_erc20_balance_of(sender);
                    async move {
                        let result = client
                            .call(
                                TransactionRequest::default()
                                    .with_to(token)
                                    .with_input(call_data)
                                    .into(),
                            )
                            .await
                            .rpc("eth_call")
                            .map(|bytes| U256::from_be_slice(bytes.as_ref()))
                            .unwrap_or(U256::ZERO);
                        (token, sender, result)
                    }
                })
            })
            .collect();

        let balance_results: Vec<_> = stream::iter(balance_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_check.inc(1))
            .collect()
            .await;
        pb_check.finish_and_clear();

        // Filter to only (token, sender) pairs that need funding.
        let mut transfers_needed: Vec<(Address, Address)> = Vec::new();
        let mut already_funded = 0usize;
        for (token, sender, balance) in balance_results {
            if balance < amount_per_token {
                transfers_needed.push((token, sender));
            } else {
                already_funded += 1;
                debug!(token = %token, sender = %sender, balance = %balance, "account already has sufficient tokens");
            }
        }

        if transfers_needed.is_empty() {
            info!(
                tokens = token_count,
                accounts = sender_addresses.len(),
                "all accounts already have sufficient token balances, skipping distribution"
            );
            return Ok(());
        }

        info!(
            transfers_needed = transfers_needed.len(),
            already_funded = already_funded,
            tokens = token_count,
            accounts = sender_addresses.len(),
            "distributing swap tokens"
        );

        // Phase 2: Setup for transfers.
        let funder_address = funding_key.address();
        let wallet = EthereumWallet::from(funding_key);
        let funder_provider =
            Arc::new(create_wallet_provider(self.config.primary_submission_rpc().clone(), wallet));
        let chain_id = self.config.chain_id;
        let max_gas_price = self.config.max_gas_price;

        let gas_price = self.client.get_gas_price().await.rpc("get gas price")?;
        let max_priority_fee = (gas_price / 10).max(1);
        let max_fee = gas_price.saturating_mul(2).max(max_priority_fee).min(max_gas_price);

        // Pre-flight balance check — abort before sending any TXs if the funder
        // cannot cover the total gas cost for needed token transfers.
        let gas_cost_per_tx = U256::from(65_000u64).saturating_mul(U256::from(max_fee));
        let total_gas_cost = gas_cost_per_tx.saturating_mul(U256::from(transfers_needed.len()));
        let funder_balance = self.client.get_balance(funder_address).await.rpc("get balance")?;

        if funder_balance < total_gas_cost {
            let shortfall = total_gas_cost.saturating_sub(funder_balance);
            return Err(BaselineError::Transaction(format!(
                "funder {} has insufficient balance for token distribution: has {} ETH, needs {} ETH (gas for {} txs), shortfall {} ETH",
                funder_address,
                format_ether(funder_balance),
                format_ether(total_gas_cost),
                transfers_needed.len(),
                format_ether(shortfall),
            )));
        }

        let mut nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .rpc("get pending transaction count")?;

        // Phase 3: Execute transfers for accounts that need tokens.
        let pb = self.progress_bar(transfers_needed.len() as u64, "Minting tokens");
        let mut failed_count: usize = 0;

        let txs: Vec<(TransactionRequest, Address, Address)> = transfers_needed
            .into_iter()
            .map(|(token, sender)| {
                let mint_data = Self::encode_erc20_mint(sender, amount_per_token);
                let tx = TransactionRequest::default()
                    .with_to(token)
                    .with_input(mint_data)
                    .with_nonce(nonce)
                    .with_chain_id(chain_id)
                    .with_gas_limit(65_000)
                    .with_max_fee_per_gas(max_fee)
                    .with_max_priority_fee_per_gas(max_priority_fee);
                nonce += 1;
                (tx, token, sender)
            })
            .collect();

        let total_txs = txs.len();
        let mut txs_remaining = txs.into_iter().peekable();
        while txs_remaining.peek().is_some() {
            let batch: Vec<_> = txs_remaining.by_ref().take(FUNDING_BATCH_SIZE).collect();
            let mut pending_txs: Vec<(Address, Address)> = Vec::new();

            let send_futs = batch.into_iter().map(|(tx, token, sender)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let result = provider.send_transaction(tx).await;
                    (result, token, sender)
                }
            });

            let mut send_stream = stream::iter(send_futs).buffer_unordered(FUNDING_BATCH_SIZE);

            while let Some((result, token, sender)) = send_stream.next().await {
                match result {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        debug!(token = %token, to = %sender, tx_hash = %tx_hash, "token mint sent");
                        pending_txs.push((token, sender));
                    }
                    Err(e) => {
                        warn!(token = %token, to = %sender, error = %e, "token mint failed");
                        failed_count += 1;
                    }
                }
            }

            Self::await_token_balances(&self.client, &mut pending_txs, amount_per_token, &pb)
                .await?;
        }

        pb.finish_and_clear();

        if failed_count > 0 {
            return Err(BaselineError::Transaction(format!(
                "{failed_count}/{total_txs} token mints failed — senders with missing tokens will revert on swap"
            )));
        }

        info!(
            tokens = token_count,
            transfers = total_txs,
            skipped = already_funded,
            "swap token setup complete"
        );
        Ok(())
    }

    fn encode_erc20_mint(to: Address, amount: U256) -> Bytes {
        sol! {
            function mint(address to, uint256 amount) external;
        }
        Bytes::from(mintCall { to, amount }.abi_encode())
    }

    fn encode_erc20_balance_of(account: Address) -> Bytes {
        sol! {
            function balanceOf(address account) external view returns (uint256);
        }
        Bytes::from(balanceOfCall { account }.abi_encode())
    }

    /// Runs the load test and returns metrics summary.
    #[instrument(skip(self), fields(target_gps = self.config.target_gps, continuous = self.config.duration.is_none(), duration = ?self.config.duration))]
    pub async fn run(&mut self) -> Result<MetricsSummary> {
        self.collector.reset();
        self.stop_flag.store(false, Ordering::SeqCst);
        self.cancel_token = CancellationToken::new();

        self.gas_price = self.client.get_gas_price().await.rpc("get gas price")?;
        info!(gas_price = self.gas_price, "fetched current gas price");

        for account in self.accounts.accounts() {
            if !self.nonce_managers.contains_key(&account.address) {
                let provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());
                let nonce_manager = NonceManager::new(provider, account.address, NONCE_RPC_TIMEOUT)
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

        const SUBMIT_CHANNEL_BUFFER: usize = 32_768;
        let (submit_event_tx, mut submit_event_rx) =
            mpsc::channel::<SubmitEvent>(SUBMIT_CHANNEL_BUFFER);

        let sender_addresses: Vec<_> = self.accounts.accounts().iter().map(|a| a.address).collect();
        let results_tracker = ResultsTracker::new(&sender_addresses);

        info!(url = %self.config.flashblocks_ws, "starting flashblock transaction watcher");
        let flashblock_watcher_task = Some(
            FlashblockWatcher::new(
                self.config.flashblocks_ws.clone(),
                results_tracker.clone(),
                self.cancel_token.clone(),
            )
            .start(),
        );

        info!(url = %self.config.query_rpc, "starting block watcher");
        let block_watcher_task = Some(
            BlockWatcher::new(
                RootProvider::<Base>::new_http(self.config.query_rpc.clone()),
                results_tracker.clone(),
                self.cancel_token.clone(),
            )
            .start(),
        );

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

        let mut pending_batch: Vec<PreparedTransaction> = Vec::with_capacity(batch_size);
        let signers = Arc::clone(&self.signers);
        let nonce_managers = Arc::clone(&self.nonce_managers);
        let submission_batch_rpcs = Arc::clone(&self.submission_batch_rpcs);
        let mut submission_pipeline = SubmissionPipeline::start(
            signers,
            nonce_managers,
            submission_batch_rpcs,
            results_tracker.clone(),
            submit_event_tx.clone(),
            self.config.chain_id,
            self.config.max_gas_price,
        );
        let next_submit_batch_id = AtomicU64::new(0);
        let mut queued_per_sender: HashMap<Address, u64> =
            self.accounts.accounts().iter().map(|a| (a.address, 0)).collect();

        let mut last_gas_price_refresh = Instant::now();
        let mut last_rate_limiter_update = Instant::now();
        let mut last_progress_report = Instant::now();
        const GAS_PRICE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
        const RATE_LIMITER_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
        const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
        const DISPLAY_RENDER_INTERVAL: Duration = Duration::from_millis(500);

        let use_live_display = self.display.as_ref().is_some_and(|d| d.is_active());
        let use_snapshot_tx = self.snapshot_tx.is_some();

        // Emit an initial snapshot immediately so the TUI renders live
        // metrics (submitted/in-flight/failed counters) without waiting
        // for the first confirmation to arrive.
        if use_live_display || use_snapshot_tx {
            let snap = self.build_snapshot(
                start,
                &results_tracker,
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
                if let Ok(new_price) = self.client.get_gas_price().await.rpc("get gas price")
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

            Self::drain_submit_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
            );

            // Drain confirmed metrics non-blocking so the rolling window stays
            // current during the run (not just during the post-run drain).
            for metrics in results_tracker.drain_confirmed_metrics() {
                self.collector.record_confirmed(metrics);
            }
            let expired = results_tracker.expire_pending(PENDING_CONFIRMATION_TIMEOUT);
            if expired > 0 {
                self.collector.record_failures("expired without confirmation", expired);
            }

            if use_live_display || use_snapshot_tx {
                if last_progress_report.elapsed() >= DISPLAY_RENDER_INTERVAL {
                    self.collector.sample_throughput(start.elapsed());
                    let snap = self.build_snapshot(
                        start,
                        &results_tracker,
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
                self.collector.sample_throughput(start.elapsed());
                let elapsed_secs = start.elapsed().as_secs();
                let submitted = self.collector.submitted_count();
                let confirmed = self.collector.confirmed_count();
                let failed = self.collector.failed_count();
                let in_flight = results_tracker.total_in_flight();
                let senders_blocked = results_tracker.senders_at_limit(max_in_flight_per_sender);
                let (p50, p99) = self.collector.rolling_p50_p99();
                let (block_receipt_delay_p50, block_receipt_delay_p99) =
                    self.collector.rolling_block_receipt_delay_p50_p99();
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
                    block_receipt_delay_p50_ms = block_receipt_delay_p50.as_millis() as u64,
                    block_receipt_delay_p99_ms = block_receipt_delay_p99.as_millis() as u64,
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
                let queued = queued_per_sender.get(&account.address).copied().unwrap_or(0);
                let sender_in_flight =
                    results_tracker.in_flight_for(&account.address).saturating_add(queued);

                if sender_in_flight >= max_in_flight_per_sender {
                    debug!(
                        sender = %account.address,
                        in_flight = sender_in_flight,
                        queued,
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

                pending_batch.push(PreparedTransaction {
                    from,
                    to: to_addr,
                    value,
                    data,
                    gas_limit,
                });
                queued_per_sender
                    .entry(from)
                    .and_modify(|count| *count = count.saturating_add(1))
                    .or_insert(1);

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
            let batch_id = next_submit_batch_id.fetch_add(1, Ordering::SeqCst);
            let batch_len = batch.len();
            let submit_batch =
                PreparedBatch { id: batch_id, gas_price: self.gas_price, txs: batch };
            match submission_pipeline.enqueue_prepared(submit_batch).await {
                Ok(()) => {
                    debug!(batch_id, batch_len, "queued submit batch");
                }
                Err(batch) => {
                    warn!(batch_id, batch_len, "submit queue closed, failing batch");
                    SubmissionPipeline::fail_prepared_batch(
                        &submit_event_tx,
                        batch.txs,
                        "submit queue closed",
                    )
                    .await;
                    rate_limiter.reset_tick();
                    break;
                }
            }
        }

        if !pending_batch.is_empty() {
            let final_batch_len = pending_batch.len();
            let batch_id = next_submit_batch_id.fetch_add(1, Ordering::SeqCst);
            let submit_batch =
                PreparedBatch { id: batch_id, gas_price: self.gas_price, txs: pending_batch };
            match submission_pipeline.enqueue_prepared(submit_batch).await {
                Ok(()) => {
                    debug!(batch_id, batch_len = final_batch_len, "queued final submit batch");
                }
                Err(batch) => {
                    warn!(batch_id, batch_len = final_batch_len, "submit queue closed");
                    SubmissionPipeline::fail_prepared_batch(
                        &submit_event_tx,
                        batch.txs,
                        "submit queue closed",
                    )
                    .await;
                }
            }
        }

        submission_pipeline.close_input();

        let drain_started = Instant::now();
        while submission_pipeline.pending_batches() > 0
            && drain_started.elapsed() < SUBMIT_DRAIN_TIMEOUT
        {
            Self::drain_submit_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let pending_submit_batches = submission_pipeline.pending_batches();
        if pending_submit_batches > 0 {
            warn!(
                pending_submit_batches,
                "timed out waiting for submit queue to drain, closing submit queue"
            );
            let failures =
                submission_pipeline.close_and_fail_queued("submit queue abandoned").await;
            Self::apply_queued_submit_failures(
                failures,
                &mut queued_per_sender,
                &mut self.collector,
            );
        }
        submission_pipeline.shutdown_and_join(SUBMIT_WORKER_SHUTDOWN_TIMEOUT).await;
        drop(submission_pipeline);

        // Close the channel so the drain below cannot miss late events.
        drop(submit_event_tx);

        Self::drain_submit_events(
            &mut submit_event_rx,
            &mut queued_per_sender,
            &mut self.collector,
        );

        // Keep background watchers alive through the drain so late flashblock
        // inclusions and block observations can still be joined into metrics.
        self.stop_flag.store(true, Ordering::SeqCst);

        if let Some(display) = &self.display {
            display.finish();
        }

        let submitted = self.collector.submitted_count();
        let in_flight = results_tracker.total_in_flight();
        let elapsed = start.elapsed();
        info!(
            submitted,
            in_flight,
            elapsed_secs = elapsed.as_secs(),
            actual_tps = submitted as f64 / elapsed.as_secs_f64(),
            "load test complete, draining confirmations"
        );

        let drain_start = Instant::now();
        let results_poll_interval = Duration::from_millis(600);
        let mut last_confirmed_at = start.elapsed();

        while drain_start.elapsed() < CONFIRMATION_DRAIN_TIMEOUT {
            let metrics = results_tracker.drain_confirmed_metrics();
            if !metrics.is_empty() {
                last_confirmed_at = start.elapsed();
                for metrics in metrics {
                    self.collector.record_confirmed(metrics);
                }
            }

            let expired = results_tracker.expire_pending(PENDING_CONFIRMATION_TIMEOUT);
            if expired > 0 {
                self.collector.record_failures("expired without confirmation", expired);
            }

            if results_tracker.pending_count() == 0 {
                break;
            }

            tokio::time::sleep(results_poll_interval).await;
        }

        for metrics in results_tracker.drain_confirmed_metrics() {
            self.collector.record_confirmed(metrics);
            last_confirmed_at = start.elapsed();
        }

        // Now safe to stop background watcher tasks.
        self.cancel_token.cancel();

        if let Some(task) = flashblock_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "flashblock watcher panicked"),
                _ => {}
            }
        }
        if let Some(task) = block_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "block watcher panicked"),
                _ => {}
            }
        }

        let confirmed = self.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        Ok(self.collector.summarize(last_confirmed_at, self.config_summary.clone()))
    }

    fn build_snapshot(
        &mut self,
        start: Instant,
        results_tracker: &ResultsTracker,
        max_in_flight_per_sender: u64,
        account_count: usize,
    ) -> DisplaySnapshot {
        let (p50, p99) = self.collector.rolling_p50_p99();
        let (block_receipt_delay_p50, block_receipt_delay_p99) =
            self.collector.rolling_block_receipt_delay_p50_p99();
        let (flashblocks_p50, flashblocks_p99) = self.collector.rolling_flashblocks_p50_p99();
        DisplaySnapshot {
            elapsed: start.elapsed(),
            duration: self.config.duration,
            submitted: self.collector.submitted_count(),
            confirmed: self.collector.confirmed_count(),
            failed: self.collector.failed_count(),
            in_flight: results_tracker.total_in_flight(),
            senders_blocked: results_tracker.senders_at_limit(max_in_flight_per_sender),
            total_senders: account_count,
            rolling_tps: self.collector.rolling_tps(),
            rolling_gps: self.collector.rolling_gps(),
            p50_latency: p50,
            p99_latency: p99,
            block_receipt_delay_p50,
            block_receipt_delay_p99,
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

    fn drain_submit_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            match event {
                SubmitEvent::Submitted(tx_hash) => collector.record_submitted(tx_hash),
                SubmitEvent::Failed(reason) => {
                    collector.record_failed(TxHash::ZERO, &reason);
                }
                SubmitEvent::Released(from) => {
                    if let Some(count) = queued_per_sender.get_mut(&from) {
                        *count = count.saturating_sub(1);
                    }
                }
            }
        }
    }

    fn apply_queued_submit_failures(
        failures: QueuedSubmitFailures,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
        for (from, released) in failures.released_by_sender {
            if let Some(count) = queued_per_sender.get_mut(&from) {
                *count = count.saturating_sub(released);
            }
        }
        if failures.failed_count > 0 {
            collector.record_failures(failures.reason, failures.failed_count);
        }
    }

    /// Drains all test account balances back to the funder address.
    ///
    /// Each account sends its entire balance minus gas costs back to the funder.
    /// Transactions that fail (e.g. zero balance) are skipped with a warning.
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn drain_accounts(&self, funding_key: PrivateKeySigner) -> Result<U256> {
        let funder_address = funding_key.address();
        let client = self.client.clone();
        let primary_submission_rpc = self.config.primary_submission_rpc().clone();
        let chain_id = self.config.chain_id;

        let gas_price = client.get_gas_price().await.rpc("get gas price")?;
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
                let primary_submission_rpc = primary_submission_rpc.clone();
                async move {
                    let balance = client
                        .get_balance(address)
                        .block_id(BlockNumberOrTag::Pending.into())
                        .await
                        .rpc("get pending balance")?;
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
                    let provider = create_wallet_provider(primary_submission_rpc, wallet);
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
                                amount = %send_amount,
                                tx_hash = %tx_hash,
                                "drain tx sent"
                            );
                            Ok(Some((address, send_amount)))
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
            if let Some((address, amount)) = result? {
                pending_txs.push(address);
                total_drained = total_drained.saturating_add(amount);
            }
        }

        if pending_txs.is_empty() {
            info!("no accounts to drain");
            return Ok(U256::ZERO);
        }

        let pb_confirm = self.progress_bar(pending_txs.len() as u64, "Waiting for drained funds");
        info!(count = pending_txs.len(), total = %total_drained, "waiting for drained balances");

        if let Err(e) =
            Self::await_drained_balances(&client, &mut pending_txs, drain_gas_cost, &pb_confirm)
                .await
        {
            warn!(error = %e, "some drain balances did not settle within timeout");
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

    /// Waits for account balances to reach a target after funding transfers.
    async fn await_balances(
        client: &QueryProvider,
        pending_accounts: &mut Vec<Address>,
        target_balance: U256,
        pb: &ProgressBar,
    ) -> Result<usize> {
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();

        let mut settled = 0usize;

        while !pending_accounts.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for address in pending_accounts.drain(..) {
                match client.get_balance(address).await.rpc("get balance") {
                    Ok(balance) if balance >= target_balance => {
                        debug!(address = %address, balance = %balance, "funding balance settled");
                        settled += 1;
                        pb.inc(1);
                    }
                    Ok(_) => {
                        still_pending.push(address);
                    }
                    Err(e) => {
                        warn!(address = %address, error = %e, "failed to check funding balance");
                        still_pending.push(address);
                    }
                }
            }
            *pending_accounts = still_pending;
        }

        if !pending_accounts.is_empty() {
            return Err(BaselineError::Transaction(format!(
                "accounts did not reach funding target within timeout: {pending_accounts:?}"
            )));
        }

        Ok(settled)
    }

    /// Waits for token balances to reach a target after mint/distribution transactions.
    async fn await_token_balances(
        client: &QueryProvider,
        pending_accounts: &mut Vec<(Address, Address)>,
        target_balance: U256,
        pb: &ProgressBar,
    ) -> Result<usize> {
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();
        let mut settled = 0usize;

        while !pending_accounts.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for (token, sender) in pending_accounts.drain(..) {
                let call_data = Self::encode_erc20_balance_of(sender);
                match client
                    .call(TransactionRequest::default().with_to(token).with_input(call_data).into())
                    .await
                    .rpc("eth_call")
                {
                    Ok(bytes) if U256::from_be_slice(bytes.as_ref()) >= target_balance => {
                        debug!(token = %token, sender = %sender, "token balance settled");
                        settled += 1;
                        pb.inc(1);
                    }
                    Ok(_) => {
                        still_pending.push((token, sender));
                    }
                    Err(e) => {
                        warn!(
                            token = %token,
                            sender = %sender,
                            error = %e,
                            "failed to check token balance"
                        );
                        still_pending.push((token, sender));
                    }
                }
            }
            *pending_accounts = still_pending;
        }

        if !pending_accounts.is_empty() {
            return Err(BaselineError::Transaction(format!(
                "token balances did not reach target within timeout: {pending_accounts:?}"
            )));
        }

        Ok(settled)
    }

    /// Waits for source account balances to drop to the post-drain dust threshold.
    async fn await_drained_balances(
        client: &QueryProvider,
        pending_accounts: &mut Vec<Address>,
        max_remaining: U256,
        pb: &ProgressBar,
    ) -> Result<usize> {
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();
        let mut settled = 0usize;

        while !pending_accounts.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for address in pending_accounts.drain(..) {
                match client.get_balance(address).await.rpc("get balance") {
                    Ok(balance) if balance <= max_remaining => {
                        debug!(address = %address, balance = %balance, "drain balance settled");
                        settled += 1;
                        pb.inc(1);
                    }
                    Ok(_) => {
                        still_pending.push(address);
                    }
                    Err(e) => {
                        warn!(address = %address, error = %e, "failed to check drain balance");
                        still_pending.push(address);
                    }
                }
            }
            *pending_accounts = still_pending;
        }

        if !pending_accounts.is_empty() {
            return Err(BaselineError::Transaction(format!(
                "accounts did not drain within timeout: {pending_accounts:?}"
            )));
        }

        Ok(settled)
    }

    /// Signals the load test to stop gracefully.
    ///
    /// Sets `stop_flag` and cancels background watcher tasks. The caller must ensure
    /// [`run()`](Self::run) completes, which handles draining confirmations.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.cancel_token.cancel();
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
            .field("signers_cached", &self.signers.len())
            .finish_non_exhaustive()
    }
}
