//! Load runner orchestration: construction, handshake, payload prep, and lifecycle controls.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, TxHash, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::NonceManager;
use rand::Rng;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument};

use super::{
    DisplaySnapshot, LoadConfig, LoadTestDisplay, LoadTestStage, SubmissionPipeline, TxType,
    ValidityRouter,
};
use crate::{
    BaselineError, Result,
    config::WorkloadConfig,
    metrics::{ConfigSummary, MetricsCollector, MetricsSummary},
    rpc::{
        BaseFeeExt, BatchRpcClient, JSON_RPC_METHOD_NOT_FOUND, QueryProvider, RpcProviders,
        RpcResultExt, create_wallet_provider,
    },
    workload::{
        AccountPool, ChainPrepContext, KeyStream, PREP_CONCURRENCY, RealTokenRecoverySummary,
        RealTokenSetup, SeededRng, WorkloadGenerator, recover_real_tokens,
    },
};

pub(super) const NONCE_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const FRESH_RECIPIENT_RNG_SALT: u64 = 0x6672_6573_685f_7263; // "fresh_rc"
const START_FILE_TIMEOUT: Duration = Duration::from_secs(300);

/// Executes load tests by generating and submitting transactions at a target rate.
pub struct LoadRunner {
    pub(super) config: LoadConfig,
    pub(super) config_summary: Option<ConfigSummary>,
    pub(super) client: QueryProvider,
    pub(super) accounts: AccountPool,
    pub(super) generator: WorkloadGenerator,
    pub(super) collector: MetricsCollector,
    pub(super) stop_flag: Arc<AtomicBool>,
    pub(super) cancel_token: CancellationToken,
    pub(super) nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
    pub(super) signers: Arc<HashMap<Address, PrivateKeySigner>>,
    pub(super) submission_batch_rpcs: Arc<Vec<BatchRpcClient>>,
    pub(super) validity_router: ValidityRouter,
    pub(super) base_fee: u128,
    pub(super) display: Option<LoadTestDisplay>,
    pub(super) snapshot_tx: Option<watch::Sender<DisplaySnapshot>>,
    /// Per-run salt for deriving each sender's own B-20 token, set during B-20 setup.
    pub(super) b20_run_salt: Option<B256>,
    pub(super) recipient_keys: Option<KeyStream>,
    pub(super) recipient_rng: SeededRng,
    /// Summary stashed when a measured run fails after submitting some work.
    pub(super) partial_summary: Option<MetricsSummary>,
}

impl LoadRunner {
    /// Creates a new load runner with the given configuration.
    #[instrument(
        skip_all,
        fields(
            submission_rpc_count = config.transaction_submission_rpcs.len(),
            chain_id = config.chain_id,
        )
    )]
    pub fn new(config: LoadConfig) -> Result<Self> {
        let requested_account_count = config.account_count;
        let config = Self::with_b20_pair_accounts(config);
        if config.account_count != requested_account_count {
            info!(
                requested = requested_account_count,
                account_count = config.account_count,
                "B-20 pairs senders; added one funded partner account"
            );
        }
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
                .map(|url| BatchRpcClient::new(url).with_batch_size(config.batch_size))
                .collect::<Vec<_>>(),
        );
        let workload_config = WorkloadConfig::new("load-test").with_seed(config.seed);
        let generator =
            WorkloadGenerator::from_tx_configs(workload_config, &config.transactions, None)?;

        info!(
            account_count = config.account_count,
            signers_cached = signers.len(),
            submission_rpc_count = submission_batch_rpcs.len(),
            "load runner created"
        );

        let recipient_keys = if config.fresh_recipient_ratio > 0.0 {
            // Fresh recipients must never repeat across runs, so the recipient stream is
            // always positioned via a runtime-random seed/offset drawn from the full valid
            // space, regardless of `fresh_recipient_ratio`. Reusing the deterministic
            // `config.seed`/sender-range offset here would regenerate the exact same
            // "fresh" addresses on every run, defeating the purpose of fresh-recipient mode.
            let sender_range_end =
                config.sender_offset.checked_add(config.account_count).ok_or_else(|| {
                    BaselineError::Config("sender_offset + account_count overflows usize".into())
                })?;
            let stream = if let Some(mnemonic) = &config.mnemonic {
                let offset_floor = u32::try_from(sender_range_end).map_err(|_| {
                    BaselineError::Config(format!(
                        "sender_offset + account_count ({sender_range_end}) exceeds u32::MAX; \
                         cannot pick a randomized mnemonic recipient index"
                    ))
                })?;
                let randomized_offset = rand::rng().random_range(offset_floor..=u32::MAX) as usize;
                let stream = KeyStream::from_mnemonic(mnemonic.clone(), randomized_offset)?;
                info!(
                    fresh_recipient_ratio = config.fresh_recipient_ratio,
                    recipient_offset_floor = sender_range_end,
                    "fresh-recipient mode enabled with randomized mnemonic offset"
                );
                stream
            } else {
                let randomized_recipient_seed: u64 = rand::rng().random();
                let stream = KeyStream::from_seed(randomized_recipient_seed, sender_range_end)?;
                info!(
                    fresh_recipient_ratio = config.fresh_recipient_ratio,
                    recipient_offset = sender_range_end,
                    "fresh-recipient mode enabled with randomized seed"
                );
                stream
            };
            Some(stream)
        } else {
            None
        };
        let recipient_rng = SeededRng::new(config.seed.wrapping_add(FRESH_RECIPIENT_RNG_SALT));
        let validity_router = ValidityRouter::new(&config);

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
            validity_router,
            base_fee: 0,
            display: None,
            snapshot_tx: None,
            b20_run_salt: None,
            recipient_keys,
            recipient_rng,
            partial_summary: None,
        })
    }

    /// Builds the workload config used to (re)construct the transaction generator.
    pub(super) fn workload_config(&self) -> WorkloadConfig {
        WorkloadConfig::new("load-test").with_seed(self.config.seed)
    }

    /// Returns instructions for recovering recipients generated in fresh-recipient mode.
    pub fn recovery_message(&self) -> Option<String> {
        self.recipient_keys.as_ref().map(KeyStream::recovery_message)
    }

    /// Returns the number of fresh recipient keys generated so far.
    pub fn fresh_recipient_count(&self) -> Option<u64> {
        self.recipient_keys.as_ref().map(KeyStream::generated_count)
    }

    /// Sets the config summary for inclusion in JSON output.
    pub fn set_config_summary(&mut self, summary: ConfigSummary) {
        self.config_summary = Some(summary);
    }

    /// Returns the number of configured txpool nodes to clear before test startup.
    pub const fn txpool_node_count(&self) -> usize {
        self.config.txpool_nodes.len()
    }

    pub(super) fn build_signers(accounts: &AccountPool) -> HashMap<Address, PrivateKeySigner> {
        accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect()
    }

    /// Probes each submission endpoint for validity-transaction support.
    ///
    /// Sends a throwaway `base_sendRawTransactionValidity` request; a
    /// method-not-found response means the node was not started with validity
    /// transactions enabled, so the run aborts loudly rather than silently
    /// degrading to plain submission. Any other response (including a rejection
    /// of the throwaway payload) confirms the method is served.
    pub(super) async fn probe_validity_endpoint(&self) -> Result<()> {
        for url in &self.config.transaction_submission_rpcs {
            let provider = RpcProviders::query(url.clone())?;
            let result: std::result::Result<TxHash, _> = provider
                .client()
                .request(
                    "base_sendRawTransactionValidity",
                    (serde_json::json!("0x"), serde_json::json!({ "validity": [] })),
                )
                .await;
            if let Err(err) = &result
                && let Some(payload) = err.as_error_resp()
                && payload.code == JSON_RPC_METHOD_NOT_FOUND
            {
                return Err(BaselineError::Config(format!(
                    "submission endpoint {url} does not serve base_sendRawTransactionValidity; \
                     start the node with --enable-experimental-validity-transactions"
                )));
            }
            debug!(url = %url, "validity endpoint capability probe passed");
        }
        Ok(())
    }

    pub(super) async fn calibrate_avg_gas(&self) -> Result<u64> {
        let total_weight: u64 =
            self.config.transactions.iter().map(|tx| u64::from(tx.weight)).sum();
        if total_weight == 0 {
            return Err(BaselineError::Config("total transaction weight must be > 0".into()));
        }

        let accounts = self.accounts.accounts();
        let mut weighted_gas = 0u128;
        for (type_index, tx_config) in self.config.transactions.iter().enumerate() {
            if tx_config.weight == 0 {
                continue;
            }

            let mut sample_config = self.config.clone();
            sample_config.transactions = vec![tx_config.clone()];
            let mut generator = WorkloadGenerator::from_tx_configs(
                self.workload_config(),
                &sample_config.transactions,
                self.b20_run_salt,
            )?;
            let sender_index = type_index % accounts.len();
            let recipient_index = if matches!(tx_config.tx_type, TxType::B20) {
                Self::b20_partner_index(sender_index, accounts.len())
            } else {
                (sender_index + 1) % accounts.len()
            };
            let account = &accounts[sender_index];
            let from = account.address;
            let to = accounts[recipient_index].address;
            let nonce = self
                .client
                .get_transaction_count(from)
                .pending()
                .await
                .rpc("get calibration transaction nonce")?;
            let base_fee = self.client.get_base_fee().await?;
            let priority_fee = (base_fee / 10).max(1);
            let max_fee = SubmissionPipeline::submission_max_fee(
                base_fee,
                priority_fee,
                self.config.max_gas_price,
            );
            let request = generator
                .generate_payload(from, to)?
                .with_from(from)
                .with_nonce(nonce)
                .with_chain_id(self.config.chain_id)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority_fee);
            let provider = create_wallet_provider(
                self.config.primary_submission_rpc().clone(),
                EthereumWallet::from(account.signer.clone()),
            );
            let receipt = provider
                .send_transaction(request)
                .await
                .rpc("submit calibration transaction")?
                .get_receipt()
                .await
                .rpc("confirm calibration transaction")?;
            if !receipt.status() {
                return Err(BaselineError::Transaction(format!(
                    "calibration transaction type {type_index} ({:?}) reverted",
                    tx_config.tx_type
                )));
            }
            let average = receipt.gas_used;
            info!(
                transaction_type_index = type_index,
                weight = tx_config.weight,
                average_gas = average,
                "calibrated transaction gas"
            );
            weighted_gas =
                weighted_gas.saturating_add(u128::from(average) * u128::from(tx_config.weight));
        }

        u64::try_from(weighted_gas / u128::from(total_weight))
            .map_err(|_| BaselineError::Config("calibrated average gas exceeds u64".into()))
    }

    pub(super) fn publish_handshake(path: Option<&Path>) -> Result<()> {
        let Some(path) = path else {
            return Ok(());
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| {
            BaselineError::Config(format!("failed to create handshake directory: {e}"))
        })?;
        let temp = parent.join(format!(".load-test-{}.tmp", std::process::id()));
        fs::write(&temp, b"ready\n")
            .and_then(|()| fs::rename(&temp, path))
            .map_err(|e| BaselineError::Config(format!("failed to publish handshake file: {e}")))
    }

    pub(super) async fn wait_for_start_file(&self) -> Result<()> {
        let Some(control_dir) = self.config.separate_setup.as_deref() else {
            return Ok(());
        };
        let path = control_dir.join("start");
        let started = Instant::now();
        while !path.exists() {
            if self.stop_flag.load(Ordering::SeqCst) || self.cancel_token.is_cancelled() {
                return Err(BaselineError::Transaction(
                    "stopped while waiting for start handshake".into(),
                ));
            }
            if started.elapsed() >= START_FILE_TIMEOUT {
                return Err(BaselineError::Timeout {
                    operation: "start handshake file".into(),
                    duration: START_FILE_TIMEOUT,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    /// Prepares configured payloads (B-20 mint, real-token balances, etc.).
    pub async fn prepare_payloads(
        &mut self,
        b20_mint: U256,
        real_token_setup: Option<&RealTokenSetup>,
    ) -> Result<()> {
        let mut ctx = ChainPrepContext {
            client: &self.client,
            accounts: &self.accounts,
            chain_id: self.config.chain_id,
            max_gas_price: self.config.max_gas_price,
            primary_submission_rpc: self.config.primary_submission_rpc().clone(),
            hide_progress: self.snapshot_tx.is_some(),
            concurrency: PREP_CONCURRENCY,
            b20_mint,
            real_token_setup,
            swap_routers: self.collect_swap_routers(),
            outputs: Default::default(),
        };
        self.generator.prepare_all(&mut ctx).await?;
        if let Some(salt) = ctx.outputs.b20_run_salt {
            self.b20_run_salt = Some(salt);
        }
        if ctx.outputs.needs_sender_refresh {
            self.refresh_sender_state().await?;
        }
        Ok(())
    }

    /// Tears down payload chain state (e.g. B-20 burns).
    pub async fn teardown_payloads(&self) -> Result<()> {
        let ctx = ChainPrepContext {
            client: &self.client,
            accounts: &self.accounts,
            chain_id: self.config.chain_id,
            max_gas_price: self.config.max_gas_price,
            primary_submission_rpc: self.config.primary_submission_rpc().clone(),
            hide_progress: self.snapshot_tx.is_some(),
            concurrency: PREP_CONCURRENCY,
            b20_mint: U256::ZERO,
            real_token_setup: None,
            swap_routers: self.collect_swap_routers(),
            outputs: crate::workload::ChainPrepOutputs {
                b20_run_salt: self.b20_run_salt,
                ..Default::default()
            },
        };
        self.generator.teardown_all(&ctx).await
    }

    /// Returns true when any configured transaction type is B-20.
    pub fn needs_b20_setup(&self) -> bool {
        self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20))
    }

    /// Ensures B-20 workloads have even sender count so every alice has a funded bob.
    fn with_b20_pair_accounts(mut config: LoadConfig) -> LoadConfig {
        if config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20))
            && config.account_count % 2 == 1
        {
            config.account_count = config.account_count.saturating_add(1);
        }
        config
    }

    /// Index of the paired B-20 counterparty (`0<->1`, `2<->3`, ...).
    ///
    /// Odd leftover senders pair with the previous account so the index stays in range.
    pub(super) const fn b20_partner_index(sender_index: usize, sender_count: usize) -> usize {
        if sender_count < 2 {
            sender_index
        } else if (sender_index ^ 1) < sender_count {
            sender_index ^ 1
        } else {
            sender_index - 1
        }
    }

    /// Recovers real-token balances before native ETH drain.
    pub async fn recover_real_tokens(
        &self,
        setup: &RealTokenSetup,
    ) -> Result<RealTokenRecoverySummary> {
        recover_real_tokens(
            &self.client,
            &self.accounts,
            self.config.chain_id,
            self.config.max_gas_price,
            self.config.primary_submission_rpc().clone(),
            self.snapshot_tx.is_some(),
            setup,
        )
        .await
    }

    /// Returns stats collected before a measured-run failure, if any.
    pub const fn take_partial_summary(&mut self) -> Option<MetricsSummary> {
        self.partial_summary.take()
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
    /// When set, the runner updates the indicatif footer every 500 ms while
    /// continuing to emit five-second structured progress events.
    pub fn set_display(&mut self, display: LoadTestDisplay) {
        self.display = Some(display);
    }

    /// Updates the live footer lifecycle stage when a display is attached.
    pub fn set_display_stage(&self, stage: LoadTestStage) {
        if let Some(display) = &self.display {
            display.set_stage(stage);
        }
    }

    /// Finishes and clears the live footer when the full lifecycle is complete.
    pub fn finish_display(&self) {
        if let Some(display) = &self.display {
            display.finish();
        }
    }

    /// Temporarily clears the live footer while synchronous output is written.
    pub fn suspend_display<T>(&self, operation: impl FnOnce() -> T) -> T {
        match &self.display {
            Some(display) => display.suspend(operation),
            None => operation(),
        }
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

#[cfg(test)]
mod tests {
    use super::{LoadConfig, LoadRunner};
    use crate::runner::{TxConfig, TxType};

    #[test]
    fn b20_odd_sender_count_adds_funded_partner() {
        let config = LoadConfig {
            account_count: 1,
            transactions: vec![TxConfig { weight: 100, tx_type: TxType::B20 }],
            ..LoadConfig::devnet()
        };
        let runner = LoadRunner::new(config).expect("valid config");
        assert_eq!(runner.accounts.len(), 2, "single B-20 sender must get a funded bob");
        assert_eq!(runner.config.account_count, 2);
    }

    #[test]
    fn b20_even_sender_count_unchanged() {
        let config = LoadConfig {
            account_count: 10,
            transactions: vec![TxConfig { weight: 100, tx_type: TxType::B20 }],
            ..LoadConfig::devnet()
        };
        let runner = LoadRunner::new(config).expect("valid config");
        assert_eq!(runner.accounts.len(), 10, "even B-20 sender count must stay unchanged");
    }

    #[test]
    fn non_b20_odd_sender_count_unchanged() {
        let config = LoadConfig { account_count: 1, ..LoadConfig::devnet() };
        let runner = LoadRunner::new(config).expect("valid config");
        assert_eq!(runner.accounts.len(), 1, "ETH transfer workloads must not add a partner");
    }

    #[test]
    fn b20_partner_index_pairs_neighbors() {
        assert_eq!(LoadRunner::b20_partner_index(0, 1), 0, "lone sender has no partner");
        assert_eq!(LoadRunner::b20_partner_index(0, 2), 1, "alice -> bob");
        assert_eq!(LoadRunner::b20_partner_index(1, 2), 0, "bob -> alice");
        assert_eq!(LoadRunner::b20_partner_index(2, 4), 3);
        assert_eq!(LoadRunner::b20_partner_index(3, 4), 2);
        assert_eq!(LoadRunner::b20_partner_index(2, 3), 1, "odd leftover pairs with previous");
    }

    #[test]
    fn fresh_recipient_seed_mode_randomizes_across_runs_even_below_ratio_one() {
        let config = LoadConfig { fresh_recipient_ratio: 0.5, ..LoadConfig::devnet() };
        let configured_seed = config.seed;

        let first = LoadRunner::new(config.clone()).expect("valid config");
        let second = LoadRunner::new(config).expect("valid config");

        let first_message = first.recovery_message().expect("fresh-recipient mode is enabled");
        let second_message = second.recovery_message().expect("fresh-recipient mode is enabled");
        assert_ne!(
            first_message, second_message,
            "recipient seed must be randomized per run, not derived from the deterministic \
             config seed, even when fresh_recipient_ratio < 1.0"
        );
        assert!(!first_message.contains(&format!("seed={configured_seed}")));
    }

    #[test]
    fn fresh_recipient_mnemonic_mode_randomizes_across_runs_even_below_ratio_one() {
        let config = LoadConfig {
            fresh_recipient_ratio: 0.3,
            mnemonic: Some(
                "test test test test test test test test test test test junk".to_string(),
            ),
            ..LoadConfig::devnet()
        };

        let first = LoadRunner::new(config.clone()).expect("valid config");
        let second = LoadRunner::new(config).expect("valid config");

        let first_message = first.recovery_message().expect("fresh-recipient mode is enabled");
        let second_message = second.recovery_message().expect("fresh-recipient mode is enabled");
        assert_ne!(
            first_message, second_message,
            "recipient mnemonic offset must be randomized per run, even when \
             fresh_recipient_ratio < 1.0"
        );
    }
}
