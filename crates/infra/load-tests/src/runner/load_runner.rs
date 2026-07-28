use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::transaction::SignableTransaction;
use alloy_eips::Encodable2718;
use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, TxHash, U256, utils::format_ether};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use base_common_network::Base;
use base_tx_manager::NonceManager;
use futures::{StreamExt, TryStreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use rand::Rng;
use tokio::{
    sync::{mpsc, watch},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

/// Maximum number of concurrent RPC requests during funding/draining operations.
pub(super) const FUNDING_CONCURRENCY: usize = 32;

use super::{
    BlockWatcher, DisplaySnapshot, FlashblockWatcher, LoadConfig, LoadTestDisplay,
    PipelineStartConfig, PreparedTransaction, QueuedSubmitFailures, ResultsTracker, SignedBatch,
    SignedTransaction, SubmissionPipeline, SubmitEvent, TxType,
};
use crate::{
    BaselineError, Result,
    config::WorkloadConfig,
    metrics::{ConfigSummary, MetricsCollector, MetricsSummary},
    rpc::{
        BaseFeeExt, BatchRpcClient, QueryProvider, RpcProviders, RpcResultExt, TxpoolAdminClient,
        create_wallet_provider,
    },
    workload::{
        AccountPool, AerodromeClPayload, B20EvmTransferPayload, B20TransferPayload,
        CalldataPayload, Erc20Payload, KeyStream, OsakaPayload, PrecompilePayload, SeededRng,
        StoragePayload, TransferPayload, UniswapV3Payload, WorkloadGenerator,
    },
};

const NONCE_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const SUBMIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const SUBMIT_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const PENDING_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(200);
const CONFIRMATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(200);
const TXPOOL_CLEAR_CONCURRENCY: usize = 64;
const FRESH_RECIPIENT_RNG_SALT: u64 = 0x6672_6573_685f_7263; // "fresh_rc"
const OPEN_LOOP_SIGNED_BATCH_SIZE: usize = 200;
const OPEN_LOOP_PRESIGN_CHANNEL_BUFFER: usize = 2;
const OPEN_LOOP_IDLE_SLEEP: Duration = Duration::from_millis(10);
const OPEN_LOOP_HEADROOM_RECHECK_INTERVAL: Duration = Duration::from_millis(100);
const OPEN_LOOP_HEADROOM_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const OPEN_LOOP_PREFILL_TIMEOUT: Duration = Duration::from_secs(300);
/// Adaptive open-loop target update cadence, matching closed-loop rate-limiter updates.
const OPEN_LOOP_TARGET_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
/// Multiplicative headroom above observed confirmed TPS when deriving open-loop depth.
const OPEN_LOOP_TARGET_MARGIN_MULTIPLIER: f64 = 1.30;
/// Conversion window from observed TPS into outstanding transaction depth.
const OPEN_LOOP_TARGET_LOOKAHEAD_SECONDS: f64 = 1.5;
/// EWMA alpha for smoothing confirmed-TPS samples and reducing target oscillation.
const OPEN_LOOP_TARGET_TPS_EWMA_ALPHA: f64 = 0.35;
/// Minimum open-loop depth, expressed in signed-batch units.
const OPEN_LOOP_TARGET_MIN_BATCHES: u64 = 1;
/// Bootstrap open-loop depth before enough confirmations exist to adapt.
const OPEN_LOOP_TARGET_INITIAL_BATCHES: u64 = 2;
const FUNDING_REPLACEMENT_FEE_MULTIPLIER: u128 = 3;
const FUNDING_REPLACEMENT_MAX_ATTEMPTS: u32 = 8;
const START_FILE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug)]
struct OpenLoopSenderJob {
    sender_index: usize,
    from: Address,
    start_nonce: u64,
    prepared_txs: Vec<PreparedTransaction>,
}

#[derive(Debug)]
struct OpenLoopSignedSender {
    sender_index: usize,
    signed_txs: Vec<SignedTransaction>,
}

#[derive(Debug)]
struct OpenLoopPresignProducerState {
    generator: WorkloadGenerator,
    recipient_keys: Option<KeyStream>,
    recipient_rng: SeededRng,
}

struct OpenLoopPresignConfig {
    sender_addresses: Vec<Address>,
    sender_next_nonces: Vec<u64>,
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    chain_id: u64,
    base_fee_rx: watch::Receiver<u128>,
    max_gas_price: u128,
    fresh_recipient_ratio: f64,
    signed_chunk_tx: mpsc::Sender<Vec<Vec<SignedTransaction>>>,
}

struct OpenLoopEnqueueProgress {
    presigned_generated: u64,
    headroom_target: OpenLoopHeadroomTarget,
}

struct OpenLoopDrainState<'a> {
    submit_event_rx: &'a mut mpsc::Receiver<SubmitEvent>,
    queued_per_sender: &'a mut HashMap<Address, u64>,
    collector: &'a mut MetricsCollector,
    results_tracker: &'a ResultsTracker,
}

impl OpenLoopDrainState<'_> {
    fn apply_submit_event(&mut self, event: SubmitEvent) {
        LoadRunner::apply_submit_event(event, self.queued_per_sender, self.collector);
    }

    fn drain_run_events(&mut self) {
        LoadRunner::drain_run_events(
            self.submit_event_rx,
            self.queued_per_sender,
            self.collector,
            self.results_tracker,
        );
    }

    /// Total outstanding work: submissions accepted by an RPC and awaiting canonical
    /// landing (`total_in_flight`) plus submissions handed to the submission pipeline
    /// but not yet accepted/rejected by an RPC (`queued_per_sender`).
    ///
    /// The headroom gate must count both, not just `total_in_flight`, otherwise a
    /// submission-pipeline backlog (e.g. a slow or saturated RPC) lets the enqueue loop
    /// race far past `target_in_flight` before the gate ever engages.
    fn total_outstanding(&self) -> u64 {
        let queued =
            self.queued_per_sender.values().fold(0u64, |total, count| total.saturating_add(*count));

        self.results_tracker.total_in_flight().saturating_add(queued)
    }
}

#[derive(Debug)]
struct OpenLoopHeadroomTarget {
    current_target_in_flight: u64,
    min_target_in_flight: u64,
    max_target_in_flight: u64,
    target_outstanding_gas: Option<u128>,
    target_gps: Option<u64>,
    initial_avg_gas: u64,
    smoothed_confirmed_tps: f64,
    last_confirmed_sample_count: u64,
    last_confirmed_sample_at: Instant,
}

#[derive(Debug)]
struct OpenLoopHeadroomUpdate {
    previous_target_in_flight: u64,
    updated_target_in_flight: u64,
    confirmed_delta: u64,
    sample_tps: f64,
    smoothed_tps: f64,
}

impl OpenLoopHeadroomTarget {
    fn new(
        max_target_in_flight: u64,
        target_gps: Option<u64>,
        initial_avg_gas: u64,
        initial_confirmed_count: u64,
        sampled_at: Instant,
    ) -> Self {
        if max_target_in_flight == 0 {
            return Self {
                current_target_in_flight: 0,
                min_target_in_flight: 0,
                max_target_in_flight: 0,
                target_outstanding_gas: None,
                target_gps,
                initial_avg_gas,
                smoothed_confirmed_tps: 0.0,
                last_confirmed_sample_count: initial_confirmed_count,
                last_confirmed_sample_at: sampled_at,
            };
        }

        let min_target_in_flight = (OPEN_LOOP_SIGNED_BATCH_SIZE as u64)
            .saturating_mul(OPEN_LOOP_TARGET_MIN_BATCHES)
            .min(max_target_in_flight);
        let initial_target_in_flight = (OPEN_LOOP_SIGNED_BATCH_SIZE as u64)
            .saturating_mul(OPEN_LOOP_TARGET_INITIAL_BATCHES)
            .clamp(min_target_in_flight, max_target_in_flight);

        let mut target = Self {
            current_target_in_flight: initial_target_in_flight,
            min_target_in_flight,
            max_target_in_flight,
            target_outstanding_gas: None,
            target_gps,
            initial_avg_gas,
            smoothed_confirmed_tps: 0.0,
            last_confirmed_sample_count: initial_confirmed_count,
            last_confirmed_sample_at: sampled_at,
        };
        target.current_target_in_flight =
            target.clamp_target_in_flight(initial_target_in_flight, None);
        target
    }

    fn saturated(
        target_outstanding_gas: u128,
        target_in_flight: u64,
        max_target_in_flight: u64,
        initial_avg_gas: u64,
        sampled_at: Instant,
    ) -> Self {
        Self {
            current_target_in_flight: target_in_flight,
            min_target_in_flight: 1,
            max_target_in_flight,
            target_outstanding_gas: Some(target_outstanding_gas),
            target_gps: None,
            initial_avg_gas,
            smoothed_confirmed_tps: 0.0,
            last_confirmed_sample_count: 0,
            last_confirmed_sample_at: sampled_at,
        }
    }

    fn gas_derived_max_in_flight(&self, avg_gas_per_tx: Option<u64>) -> Option<u64> {
        let cap = self.target_gps?;
        let avg_gas = avg_gas_per_tx.unwrap_or(self.initial_avg_gas).max(1);
        Some(cap / avg_gas)
    }

    fn clamp_target_in_flight(&self, target_in_flight: u64, avg_gas_per_tx: Option<u64>) -> u64 {
        let clamped_to_bounds =
            target_in_flight.clamp(self.min_target_in_flight, self.max_target_in_flight);
        self.gas_derived_max_in_flight(avg_gas_per_tx)
            .map_or(clamped_to_bounds, |gas_derived_max| clamped_to_bounds.min(gas_derived_max))
    }

    const fn current_target_in_flight(&self) -> u64 {
        self.current_target_in_flight
    }

    fn maybe_update(
        &mut self,
        now: Instant,
        confirmed_count: u64,
        avg_gas_per_tx: Option<u64>,
    ) -> Option<OpenLoopHeadroomUpdate> {
        if now.saturating_duration_since(self.last_confirmed_sample_at)
            < OPEN_LOOP_TARGET_UPDATE_INTERVAL
        {
            return None;
        }

        let elapsed = now.saturating_duration_since(self.last_confirmed_sample_at).as_secs_f64();
        let confirmed_delta = confirmed_count.saturating_sub(self.last_confirmed_sample_count);
        let sample_tps = if elapsed > 0.0 { confirmed_delta as f64 / elapsed } else { 0.0 };

        if let Some(target_gas) = self.target_outstanding_gas {
            let average_gas = u128::from(avg_gas_per_tx.unwrap_or(self.initial_avg_gas).max(1));
            let updated_target_in_flight = u64::try_from(target_gas.div_ceil(average_gas))
                .unwrap_or(u64::MAX)
                .clamp(self.min_target_in_flight, self.max_target_in_flight);
            let previous_target_in_flight = self.current_target_in_flight;
            self.current_target_in_flight = updated_target_in_flight;
            self.last_confirmed_sample_count = confirmed_count;
            self.last_confirmed_sample_at = now;
            return Some(OpenLoopHeadroomUpdate {
                previous_target_in_flight,
                updated_target_in_flight,
                confirmed_delta,
                sample_tps,
                smoothed_tps: sample_tps,
            });
        }

        if self.smoothed_confirmed_tps == 0.0 {
            self.smoothed_confirmed_tps = sample_tps;
        } else {
            self.smoothed_confirmed_tps = self.smoothed_confirmed_tps
                * (1.0 - OPEN_LOOP_TARGET_TPS_EWMA_ALPHA)
                + sample_tps * OPEN_LOOP_TARGET_TPS_EWMA_ALPHA;
        }

        let adaptive_target = (self.smoothed_confirmed_tps
            * OPEN_LOOP_TARGET_MARGIN_MULTIPLIER
            * OPEN_LOOP_TARGET_LOOKAHEAD_SECONDS)
            .ceil() as u64;
        let target_with_batch_buffer =
            adaptive_target.saturating_add(OPEN_LOOP_SIGNED_BATCH_SIZE as u64);
        let updated_target_in_flight =
            self.clamp_target_in_flight(target_with_batch_buffer, avg_gas_per_tx);
        let previous_target_in_flight = self.current_target_in_flight;

        self.current_target_in_flight = updated_target_in_flight;
        self.last_confirmed_sample_count = confirmed_count;
        self.last_confirmed_sample_at = now;

        Some(OpenLoopHeadroomUpdate {
            previous_target_in_flight,
            updated_target_in_flight,
            confirmed_delta,
            sample_tps,
            smoothed_tps: self.smoothed_confirmed_tps,
        })
    }
}

/// Executes load tests by generating and submitting transactions at a target rate.
pub struct LoadRunner {
    pub(super) config: LoadConfig,
    config_summary: Option<ConfigSummary>,
    pub(super) client: QueryProvider,
    pub(super) accounts: AccountPool,
    pub(super) generator: WorkloadGenerator,
    collector: MetricsCollector,
    stop_flag: Arc<AtomicBool>,
    cancel_token: CancellationToken,
    nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    submission_batch_rpcs: Arc<Vec<BatchRpcClient>>,
    base_fee: u128,
    display: Option<LoadTestDisplay>,
    snapshot_tx: Option<watch::Sender<DisplaySnapshot>>,
    last_total_eth: Option<String>,
    last_min_eth: Option<String>,
    last_funds_low: bool,
    funder_address: Option<String>,
    sender_addresses: Vec<String>,
    /// Per-run salt for deriving each sender's own B-20 token, set during B-20 setup.
    pub(super) b20_run_salt: Option<B256>,
    recipient_keys: Option<KeyStream>,
    recipient_rng: SeededRng,
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
        let generator = Self::create_generator(workload_config, &config, None)?;

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
                    randomized_recipient_offset = randomized_offset,
                    "fresh-recipient mode enabled with per-run randomized mnemonic offset; recover addresses with AccountPool::from_mnemonic(mnemonic, n, randomized_recipient_offset)",
                );
                stream
            } else {
                let randomized_recipient_seed: u64 = rand::rng().random();
                let stream = KeyStream::from_seed(randomized_recipient_seed, sender_range_end)?;
                info!(
                    configured_seed = config.seed,
                    randomized_recipient_seed,
                    fresh_recipient_ratio = config.fresh_recipient_ratio,
                    recipient_offset = sender_range_end,
                    "fresh-recipient mode enabled with per-run randomized seed; recover addresses with AccountPool::with_offset(randomized_recipient_seed, n, recipient_offset)",
                );
                stream
            };
            Some(stream)
        } else {
            None
        };
        let recipient_rng = SeededRng::new(config.seed.wrapping_add(FRESH_RECIPIENT_RNG_SALT));

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
            base_fee: 0,
            display: None,
            snapshot_tx: None,
            last_total_eth: None,
            last_min_eth: None,
            last_funds_low: false,
            funder_address: None,
            sender_addresses,
            b20_run_salt: None,
            recipient_keys,
            recipient_rng,
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

    pub(super) fn build_signers(accounts: &AccountPool) -> HashMap<Address, PrivateKeySigner> {
        accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect()
    }

    pub(super) fn create_generator(
        workload_config: WorkloadConfig,
        config: &LoadConfig,
        b20_run_salt: Option<B256>,
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
                TxType::Storage { contract, slots_per_tx } => {
                    generator = generator
                        .with_payload(StoragePayload::new(*contract, *slots_per_tx), weight_pct);
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
                TxType::B20 => {
                    // Each sender transfers its own per-run token; the payload derives the token
                    // from the run salt, which is only known after B-20 setup runs. Before setup
                    // (salt None) the payload is intentionally not installed.
                    if let Some(run_salt) = b20_run_salt {
                        generator = generator.with_payload(
                            B20TransferPayload::new(run_salt, U256::from(1), U256::from(1)),
                            weight_pct,
                        );
                    }
                }
                TxType::B20Evm { contract } => {
                    generator = generator.with_payload(
                        B20EvmTransferPayload::new(*contract, U256::from(1), U256::from(1)),
                        weight_pct,
                    );
                }
                TxType::Osaka { target } => {
                    generator =
                        generator.with_payload(OsakaPayload::new(target.clone()), weight_pct);
                }
                TxType::UniswapV3 {
                    router,
                    token_in,
                    token_out,
                    fee,
                    min_amount,
                    max_amount,
                    reverse_min_amount,
                    reverse_max_amount,
                } => {
                    generator = generator.with_payload(
                        UniswapV3Payload::new(
                            *router,
                            *token_in,
                            *token_out,
                            *fee,
                            *min_amount,
                            *max_amount,
                            Some((*reverse_min_amount, *reverse_max_amount)),
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
                    reverse_min_amount,
                    reverse_max_amount,
                } => {
                    generator = generator.with_payload(
                        AerodromeClPayload::new(
                            *router,
                            *token_in,
                            *token_out,
                            *tick_spacing,
                            *min_amount,
                            *max_amount,
                            Some((*reverse_min_amount, *reverse_max_amount)),
                        ),
                        weight_pct,
                    );
                }
            }
        }

        Ok(generator)
    }

    async fn calibrate_avg_gas(&self) -> Result<u64> {
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
            let mut generator =
                Self::create_generator(self.workload_config(), &sample_config, self.b20_run_salt)?;
            let sender_index = type_index % accounts.len();
            let recipient_index = (sender_index + 1) % accounts.len();
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

    /// Computes the fixed transaction inventory required to hold the requested gas.
    pub fn mempool_target_transactions(
        block_gas_limit: u64,
        target_blocks: u64,
        average_gas: u64,
        capacity: u64,
    ) -> Result<u64> {
        if average_gas == 0 {
            return Err(BaselineError::Config("calibrated average gas must be > 0".into()));
        }
        let gas = u128::from(block_gas_limit)
            .checked_mul(u128::from(target_blocks))
            .ok_or_else(|| BaselineError::Config("mempool gas target overflowed".into()))?;
        let target = gas.div_ceil(u128::from(average_gas));
        let target = u64::try_from(target)
            .map_err(|_| BaselineError::Config("mempool transaction target exceeds u64".into()))?;
        if target > capacity {
            return Err(BaselineError::Config(format!(
                "mempool target requires {target} transactions but sender capacity is {capacity}"
            )));
        }
        Ok(target)
    }

    fn publish_handshake(path: Option<&Path>) -> Result<()> {
        let Some(path) = path else { return Ok(()) };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| {
            BaselineError::Config(format!("failed to create handshake directory: {e}"))
        })?;
        let temp = parent.join(format!(".load-test-{}.tmp", std::process::id()));
        fs::write(&temp, b"ready\n")
            .and_then(|()| fs::rename(&temp, path))
            .map_err(|e| BaselineError::Config(format!("failed to publish handshake file: {e}")))
    }

    async fn wait_for_start_file(&self) -> Result<()> {
        let Some(control_dir) = self.config.separate_setup.as_deref() else { return Ok(()) };
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

        let initial_base_fee = client.get_base_fee().await?;
        let initial_priority_fee = (initial_base_fee / 10).max(1);
        let initial_max_fee = SubmissionPipeline::submission_max_fee(
            initial_base_fee,
            initial_priority_fee,
            max_gas_price,
        );

        // Phase 2: Early balance validation — abort before sending any TXs if
        // the funder cannot cover the total cost.
        let total_deficit: U256 = accounts_to_fund
            .iter()
            .map(|(_, deficit)| *deficit)
            .fold(U256::ZERO, |a, b| a.saturating_add(b));
        let gas_cost_per_tx = U256::from(21_000u64).saturating_mul(U256::from(initial_max_fee));
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

        // Phase 3+4: Send funding TXs in batches and confirm each batch before
        // sending the next. This avoids overwhelming the txpool's per-sender limit.
        let funding_requests: Vec<(Address, U256, u64)> = accounts_to_fund
            .iter()
            .enumerate()
            .map(|(i, &(address, deficit))| {
                let nonce = start_nonce
                    .checked_add(u64::try_from(i).expect("account index exceeds u64"))
                    .expect("nonce overflow");
                (address, deficit, nonce)
            })
            .collect();

        let total_txs = funding_requests.len() as u64;
        let pb_fund = self.progress_bar(total_txs, "Funding accounts");
        let mut txs_remaining = funding_requests.into_iter().peekable();
        while txs_remaining.peek().is_some() {
            let base_fee = client.get_base_fee().await?;
            let max_priority_fee = (base_fee / 10).max(1);
            // Funding transactions form one strict nonce chain and may wait several blocks behind
            // earlier transfers. Use the configured safety cap as maxFeePerGas so a rising base
            // fee cannot make the head transaction non-executable and strand every later nonce.
            // EIP-1559 still charges only base fee plus priority fee, not this full cap.
            let max_fee = max_gas_price;
            info!(base_fee, max_fee, max_priority_fee, "pricing funding transaction batch");
            let batch: Vec<_> =
                txs_remaining.by_ref().take(self.config.funding_batch_size).collect();
            let mut batch_pending: Vec<Address> = Vec::with_capacity(batch.len());
            let mut retries: Vec<(Address, U256, u64)> = Vec::new();
            let mut fatal_errors: Vec<String> = Vec::new();

            let send_futs = batch.into_iter().map(|(address, deficit, nonce)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let tx = TransactionRequest::default()
                        .with_to(address)
                        .with_value(deficit)
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(21_000)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);
                    let result = provider.send_transaction(tx).await;
                    (result, address, deficit, nonce)
                }
            });

            let mut send_stream =
                stream::iter(send_futs).buffer_unordered(self.config.funding_batch_size);

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
                            info!(to = %address, nonce, "funding transaction already pending");
                            batch_pending.push(address);
                        } else if error_str.contains("replacement transaction underpriced") {
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
                let replacement_addresses: Vec<Address> =
                    retries.iter().map(|(address, _, _)| *address).collect();
                let retry_futs = retries.into_iter().map(|(address, deficit, nonce)| {
                    let provider = Arc::clone(&funder_provider);
                    async move {
                        let mut replacement_max_fee = max_fee;
                        let mut replacement_priority_fee = max_priority_fee;

                        for attempt in 1..=FUNDING_REPLACEMENT_MAX_ATTEMPTS {
                            let next_max_fee = replacement_max_fee
                                .saturating_mul(FUNDING_REPLACEMENT_FEE_MULTIPLIER)
                                .min(max_gas_price);
                            let next_priority_fee = replacement_priority_fee
                                .saturating_mul(FUNDING_REPLACEMENT_FEE_MULTIPLIER)
                                .min(next_max_fee);
                            if next_max_fee == replacement_max_fee
                                && next_priority_fee == replacement_priority_fee
                            {
                                return Err(format!(
                                    "replacement funding tx for {address} nonce {nonce} remains underpriced at max gas price {max_gas_price}"
                                ));
                            }
                            replacement_max_fee = next_max_fee;
                            replacement_priority_fee = next_priority_fee;

                            let replacement = TransactionRequest::default()
                                .with_to(address)
                                .with_value(deficit)
                                .with_nonce(nonce)
                                .with_chain_id(chain_id)
                                .with_gas_limit(21_000)
                                .with_max_fee_per_gas(replacement_max_fee)
                                .with_max_priority_fee_per_gas(replacement_priority_fee);

                            match provider.send_transaction(replacement).await {
                                Ok(pending) => {
                                    return Ok((address, nonce, Some(*pending.tx_hash()), attempt));
                                }
                                Err(e) => {
                                    let error = e.to_string();
                                    if error.contains("already known")
                                        || error.contains("nonce too low")
                                    {
                                        return Ok((address, nonce, None, attempt));
                                    }
                                    if !error.contains("replacement transaction underpriced") {
                                        return Err(format!(
                                            "replacement funding tx for {address} nonce {nonce} failed: {e}"
                                        ));
                                    }
                                    warn!(
                                        to = %address,
                                        nonce,
                                        attempt,
                                        replacement_max_fee,
                                        replacement_priority_fee,
                                        "replacement funding transaction still underpriced"
                                    );
                                }
                            }
                        }

                        Err(format!(
                            "replacement funding tx for {address} nonce {nonce} remained underpriced after {FUNDING_REPLACEMENT_MAX_ATTEMPTS} attempts"
                        ))
                    }
                });

                let mut retry_stream =
                    stream::iter(retry_futs).buffer_unordered(self.config.funding_batch_size);

                while let Some(result) = retry_stream.next().await {
                    match result {
                        Ok((address, nonce, tx_hash, attempt)) => {
                            info!(
                                to = %address,
                                nonce,
                                attempt,
                                tx_hash = ?tx_hash,
                                "replacement funding transaction accepted"
                            );
                        }
                        Err(error) => {
                            fatal_errors.push(error);
                        }
                    }
                }

                if !fatal_errors.is_empty() {
                    pb_fund.finish_and_clear();
                    return Err(BaselineError::Transaction(format!(
                        "{} replacement funding tx(s) failed: {}",
                        fatal_errors.len(),
                        fatal_errors.join("; "),
                    )));
                }

                // Every replacement either entered the pool, was already known, or its nonce was
                // consumed while retrying. In all cases, wait for the intended recipient balance
                // before allowing later funder nonces to proceed.
                batch_pending.extend(replacement_addresses);
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
                    stream::iter(nonce_retry_futs).buffered(self.config.funding_batch_size);

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

        let refresh_provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());

        for result in refresh_results {
            let (addr, balance, account_nonce) = result?;
            let idx = addr_to_idx[&addr];
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = account_nonce;

            let nonce_manager =
                NonceManager::new(refresh_provider.clone(), addr, NONCE_RPC_TIMEOUT)
                    .with_pending_tag();
            Arc::make_mut(&mut self.nonce_managers).insert(addr, nonce_manager);

            debug!(address = %addr, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        info!(funded = accounts_to_fund.len(), "funding complete");
        Ok(())
    }

    /// Collects unique token addresses from configured swap transaction types.
    pub fn collect_swap_tokens(&self) -> Vec<Address> {
        let mut tokens = HashSet::new();
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
                | TxType::Storage { .. }
                | TxType::B20
                | TxType::B20Evm { .. }
                | TxType::Precompile { .. }
                | TxType::Osaka { .. } => {}
            }
        }
        tokens.into_iter().collect()
    }

    /// Collects unique router addresses from configured swap transaction types.
    pub fn collect_swap_routers(&self) -> Vec<Address> {
        let mut routers = HashSet::new();
        for tx_config in &self.config.transactions {
            match &tx_config.tx_type {
                TxType::UniswapV3 { router, .. } | TxType::AerodromeCl { router, .. } => {
                    routers.insert(*router);
                }
                TxType::Transfer
                | TxType::Calldata { .. }
                | TxType::Erc20 { .. }
                | TxType::Storage { .. }
                | TxType::B20
                | TxType::B20Evm { .. }
                | TxType::Precompile { .. }
                | TxType::Osaka { .. } => {}
            }
        }
        routers.into_iter().collect()
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

        let base_fee = self.client.get_base_fee().await?;
        let max_priority_fee = (base_fee / 10).max(1);
        let max_fee =
            SubmissionPipeline::submission_max_fee(base_fee, max_priority_fee, max_gas_price);

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
            let batch: Vec<_> =
                txs_remaining.by_ref().take(self.config.funding_batch_size).collect();
            let mut pending_txs: Vec<(Address, Address)> = Vec::new();

            let send_futs = batch.into_iter().map(|(tx, token, sender)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let result = provider.send_transaction(tx).await;
                    (result, token, sender)
                }
            });

            let mut send_stream =
                stream::iter(send_futs).buffer_unordered(self.config.funding_batch_size);

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

    pub(super) fn encode_erc20_balance_of(account: Address) -> Bytes {
        sol! {
            function balanceOf(address account) external view returns (uint256);
        }
        Bytes::from(balanceOfCall { account }.abi_encode())
    }

    /// Runs the load test and returns metrics summary.
    #[instrument(skip(self), fields(target_gps = ?self.config.target_gps, continuous = self.config.duration.is_none(), duration = ?self.config.duration))]
    pub async fn run(&mut self) -> Result<MetricsSummary> {
        if self.b20_run_salt.is_none()
            && self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20))
        {
            return Err(BaselineError::Config(
                "b20 run salt not set; call setup_b20_tokens before run".into(),
            ));
        }

        self.collector.reset();
        self.stop_flag.store(false, Ordering::SeqCst);
        self.cancel_token = CancellationToken::new();

        self.base_fee = self.client.get_base_fee().await?;
        info!(base_fee = self.base_fee, "fetched current base fee");

        for account in self.accounts.accounts() {
            if !self.nonce_managers.contains_key(&account.address) {
                let provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());
                let nonce_manager = NonceManager::new(provider, account.address, NONCE_RPC_TIMEOUT)
                    .with_pending_tag();
                Arc::make_mut(&mut self.nonce_managers).insert(account.address, nonce_manager);
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
        let receipt_provider = RootProvider::<Base>::new_http(self.config.query_rpc.clone());
        let block_watcher_task = Some(
            BlockWatcher::new(
                receipt_provider.clone(),
                results_tracker.clone(),
                self.cancel_token.clone(),
            )
            .start(),
        );

        let max_in_flight_per_sender = self.config.max_in_flight_per_sender;

        let initial_avg_gas = self.calibrate_avg_gas().await?;
        // Seed the collector so live throughput (rolling GPS) and rate-limiter
        // feedback have a non-zero gas figure before canonical receipt gas lands.
        self.collector.set_estimated_gas(initial_avg_gas);
        let mut start = Instant::now();
        let account_count = self.accounts.len();

        info!(
            sender_count = account_count,
            max_sender_workers =
                SubmissionPipeline::sender_worker_count(self.submission_batch_rpcs.len()),
            max_in_flight_per_sender,
            initial_avg_gas,
            target_gps = self
                .config
                .target_gps
                .map_or_else(|| "unbounded".to_string(), |gps| format!("{gps} gas/s")),
            "starting load test in open-loop pre-signed mode"
        );

        let signers = Arc::clone(&self.signers);
        let nonce_managers = Arc::clone(&self.nonce_managers);
        let submission_batch_rpcs = Arc::clone(&self.submission_batch_rpcs);
        let mut submission_pipeline = SubmissionPipeline::start(
            signers,
            nonce_managers,
            submission_batch_rpcs,
            results_tracker.clone(),
            submit_event_tx.clone(),
            PipelineStartConfig {
                chain_id: self.config.chain_id,
                max_gas_price: self.config.max_gas_price,
            },
        );
        let next_submit_batch_id = AtomicU64::new(0);
        let mut queued_per_sender: HashMap<Address, u64> =
            self.accounts.accounts().iter().map(|a| (a.address, 0)).collect();

        let mut last_base_fee_refresh = Instant::now();
        let mut last_progress_report = Instant::now();
        let mut last_submitted_sample_count = 0u64;
        let mut last_submitted_sample_at = Instant::now();
        // Refresh once per block so the cached base fee tracks the climb the load
        // test itself induces; a stale fee mints underwater (unincludable) txs.
        const BASE_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
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

        let mut open_loop_enqueue_error: Option<BaselineError> = None;
        let pre_sign_started = Instant::now();
        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|account| account.address).collect();
        let sender_start_nonces = self.open_loop_sender_start_nonces(&sender_addresses).await?;
        let sender_count = sender_addresses.len();

        let capacity = max_in_flight_per_sender.saturating_mul(account_count as u64);
        let open_loop_headroom_target = if self.config.target_gps.is_some() {
            OpenLoopHeadroomTarget::new(
                capacity,
                self.config.target_gps,
                initial_avg_gas,
                self.collector.confirmed_count() as u64,
                Instant::now(),
            )
        } else {
            let block_gas_limit = if let Some(limit) = self.config.block_gas_limit {
                limit
            } else {
                self.client
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .hashes()
                    .await
                    .map_err(|e| {
                        BaselineError::Rpc(format!("failed to read latest block gas limit: {e}"))
                    })?
                    .ok_or_else(|| BaselineError::Rpc("latest block is unavailable".into()))?
                    .header
                    .gas_limit
            };
            let target = Self::mempool_target_transactions(
                block_gas_limit,
                self.config.mempool_target_blocks,
                initial_avg_gas,
                capacity,
            )?;
            let target_gas =
                u128::from(block_gas_limit) * u128::from(self.config.mempool_target_blocks);
            OpenLoopHeadroomTarget::saturated(
                target_gas,
                target,
                capacity,
                initial_avg_gas,
                Instant::now(),
            )
        };
        let initial_target_in_flight = open_loop_headroom_target.current_target_in_flight();

        let replacement_generator =
            Self::create_generator(self.workload_config(), &self.config, self.b20_run_salt)?;
        let producer_generator = std::mem::replace(&mut self.generator, replacement_generator);
        let producer_recipient_keys = self.recipient_keys.take();
        let producer_recipient_rng = std::mem::take(&mut self.recipient_rng);

        let (signed_chunk_tx, mut signed_chunk_rx) =
            mpsc::channel(OPEN_LOOP_PRESIGN_CHANNEL_BUFFER);
        let (base_fee_tx, base_fee_rx) = watch::channel(self.base_fee);
        let base_fee_client = self.client.clone();
        let base_fee_cancel = self.cancel_token.clone();
        let base_fee_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = base_fee_cancel.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(1)) => {
                        if let Ok(base_fee) = base_fee_client.get_base_fee().await {
                            base_fee_tx.send_replace(base_fee);
                        }
                    }
                }
            }
        });
        let producer_task = tokio::spawn(Self::stream_open_loop_presigned_transactions(
            OpenLoopPresignProducerState {
                generator: producer_generator,
                recipient_keys: producer_recipient_keys,
                recipient_rng: producer_recipient_rng,
            },
            OpenLoopPresignConfig {
                sender_addresses,
                sender_next_nonces: sender_start_nonces,
                signers: Arc::clone(&self.signers),
                chain_id: self.config.chain_id,
                base_fee_rx,
                max_gas_price: self.config.max_gas_price,
                fresh_recipient_ratio: self.config.fresh_recipient_ratio,
                signed_chunk_tx,
            },
        ));

        let mut progress = OpenLoopEnqueueProgress {
            presigned_generated: 0,
            headroom_target: open_loop_headroom_target,
        };

        info!(
            sender_count,
            initial_target_in_flight,
            max_target_in_flight = capacity,
            mempool_target_blocks = self.config.mempool_target_blocks,
            "started open-loop streaming pre-sign pipeline"
        );

        let prefill_deadline = Instant::now() + OPEN_LOOP_PREFILL_TIMEOUT;
        let mut prefill_result = Self::enqueue_open_loop_signed_transactions(
            &submission_pipeline,
            &next_submit_batch_id,
            &mut signed_chunk_rx,
            &mut progress,
            Some(prefill_deadline),
            true,
            &self.stop_flag,
            &mut OpenLoopDrainState {
                submit_event_rx: &mut submit_event_rx,
                queued_per_sender: &mut queued_per_sender,
                collector: &mut self.collector,
                results_tracker: &results_tracker,
            },
        )
        .await;

        if prefill_result.is_ok() {
            let drain_started = Instant::now();
            while submission_pipeline.pending_batches() > 0
                && drain_started.elapsed() < SUBMIT_DRAIN_TIMEOUT
            {
                Self::drain_run_events(
                    &mut submit_event_rx,
                    &mut queued_per_sender,
                    &mut self.collector,
                    &results_tracker,
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
                &results_tracker,
            );
            let pending_batches = submission_pipeline.pending_batches();
            if pending_batches > 0 {
                prefill_result = Err(BaselineError::Timeout {
                    operation: format!(
                        "setup submission pipeline drain ({pending_batches} batches pending)"
                    ),
                    duration: SUBMIT_DRAIN_TIMEOUT,
                });
            }
        }

        if prefill_result.is_ok() {
            let ready_file = self.config.separate_setup.as_deref().map(|dir| dir.join("ready"));
            Self::publish_handshake(ready_file.as_deref())?;
            self.wait_for_start_file().await?;
            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
                &results_tracker,
            );
            results_tracker.begin_measurement();
            self.collector.reset();
            self.collector.set_estimated_gas(initial_avg_gas);
            start = Instant::now();
            let started_file = self.config.separate_setup.as_deref().map(|dir| dir.join("started"));
            Self::publish_handshake(started_file.as_deref())?;
        }

        let enqueue_deadline = self.config.duration.map(|d| start + d);
        let enqueue_result = if let Err(err) = prefill_result {
            Err(err)
        } else {
            Self::enqueue_open_loop_signed_transactions(
                &submission_pipeline,
                &next_submit_batch_id,
                &mut signed_chunk_rx,
                &mut progress,
                enqueue_deadline,
                false,
                &self.stop_flag,
                &mut OpenLoopDrainState {
                    submit_event_rx: &mut submit_event_rx,
                    queued_per_sender: &mut queued_per_sender,
                    collector: &mut self.collector,
                    results_tracker: &results_tracker,
                },
            )
            .await
        };

        let finished_file = self.config.separate_setup.as_deref().map(|dir| dir.join("finished"));
        Self::publish_handshake(finished_file.as_deref())?;

        drop(signed_chunk_rx);

        match producer_task.await {
            Ok(Ok(producer_state)) => {
                self.generator = producer_state.generator;
                self.recipient_keys = producer_state.recipient_keys;
                self.recipient_rng = producer_state.recipient_rng;
            }
            Ok(Err(err)) => {
                warn!(error = %err, "open-loop pre-sign producer failed");
                if open_loop_enqueue_error.is_none() {
                    open_loop_enqueue_error = Some(err);
                }
            }
            Err(join_error) => {
                let err = BaselineError::Transaction(format!(
                    "open-loop pre-sign producer task failed: {join_error}"
                ));
                warn!(error = %err, "open-loop pre-sign producer task failed");
                if open_loop_enqueue_error.is_none() {
                    open_loop_enqueue_error = Some(err);
                }
            }
        }
        base_fee_task.abort();

        if let Err(err) = enqueue_result {
            warn!(
                error = %err,
                presigned_generated = progress.presigned_generated,
                "open-loop enqueue aborted; proceeding to drain and shutdown"
            );
            open_loop_enqueue_error = Some(err);
        } else {
            info!(
                presigned_generated = progress.presigned_generated,
                elapsed_secs = pre_sign_started.elapsed().as_secs_f64(),
                "open-loop pre-signed enqueue phase complete"
            );
        }

        while self.config.duration.is_none_or(|d| start.elapsed() < d)
            && !self.stop_flag.load(Ordering::SeqCst)
            && open_loop_enqueue_error.is_none()
        {
            // --- Housekeeping (runs once per batch iteration) ---

            if last_base_fee_refresh.elapsed() >= BASE_FEE_REFRESH_INTERVAL {
                if let Ok(new_base_fee) = self.client.get_base_fee().await
                    && new_base_fee != self.base_fee
                {
                    debug!(old_base_fee = self.base_fee, new_base_fee, "base fee updated");
                    self.base_fee = new_base_fee;
                }
                last_base_fee_refresh = Instant::now();
            }

            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
                &results_tracker,
            );

            // Drain flashblock observations for the rolling window (separate from
            // confirmed metrics to avoid double-counting in the final summary).
            for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
                self.collector.record_flashblock_observed(latency, observed_at);
            }
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
                let pending = results_tracker.pending_count();
                let senders_blocked = results_tracker.senders_at_limit(max_in_flight_per_sender);
                let total_queued: u64 = queued_per_sender.values().sum();
                let (p50, p99) = self.collector.rolling_p50_p99();
                let (flashblocks_p50, flashblocks_p99) =
                    self.collector.rolling_flashblocks_p50_p99();
                let report_now = Instant::now();
                let report_elapsed_secs =
                    report_now.saturating_duration_since(last_submitted_sample_at).as_secs_f64();
                let submitted_delta = submitted.saturating_sub(last_submitted_sample_count);
                let submitted_per_sec = if report_elapsed_secs > 0.0 {
                    submitted_delta as f64 / report_elapsed_secs
                } else {
                    0.0
                };
                info!(
                    elapsed_secs,
                    submitted,
                    submitted_per_sec,
                    confirmed,
                    failed,
                    in_flight,
                    pending,
                    total_queued,
                    senders_blocked,
                    presigned_generated = progress.presigned_generated,
                    base_fee = self.base_fee,
                    p50_ms = p50.as_millis() as u64,
                    p99_ms = p99.as_millis() as u64,
                    flashblocks_p50_ms = flashblocks_p50.as_millis() as u64,
                    flashblocks_p99_ms = flashblocks_p99.as_millis() as u64,
                    "progress"
                );
                last_submitted_sample_count = submitted;
                last_submitted_sample_at = report_now;
                last_progress_report = Instant::now();
            }

            tokio::time::sleep(OPEN_LOOP_IDLE_SLEEP).await;
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
        let confirmation_drain_timeout = if open_loop_enqueue_error.is_some() {
            warn!("skipping confirmation drain after open-loop enqueue failure");
            Duration::ZERO
        } else {
            CONFIRMATION_DRAIN_TIMEOUT
        };
        let results_poll_interval = Duration::from_millis(600);
        let mut last_confirmed_at = start.elapsed();

        while drain_start.elapsed() < confirmation_drain_timeout {
            for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
                self.collector.record_flashblock_observed(latency, observed_at);
            }
            let metrics = results_tracker.drain_confirmed_metrics();
            if !metrics.is_empty() {
                last_confirmed_at = start.elapsed();
                for metrics in metrics {
                    self.collector.record_confirmed(metrics);
                }
            }

            // Use a shorter expiry during drain: the test is over, so any
            // pending tx older than the drain window itself is stale.
            let drain_expiry = PENDING_CONFIRMATION_TIMEOUT.saturating_sub(drain_start.elapsed());
            let expired = results_tracker.expire_pending(drain_expiry);
            if expired > 0 {
                self.collector.record_failures("expired without confirmation", expired);
            }

            if results_tracker.pending_count() == 0 {
                break;
            }

            tokio::time::sleep(results_poll_interval).await;
        }

        for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
            self.collector.record_flashblock_observed(latency, observed_at);
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

        // Fetch canonical receipts in a single batch pass, scoped to only the blocks
        // our transactions landed in, to backfill gas and revert status. This can be
        // slow on large runs, so notify the user before starting.
        let landed_blocks = results_tracker.landed_block_numbers();
        if open_loop_enqueue_error.is_none() && !landed_blocks.is_empty() {
            println!(
                "Fetching receipts for {} block(s) to compute gas and reverts (this may take a while)...",
                landed_blocks.len()
            );
            let receipt_fetch_start = Instant::now();
            let (receipts, failed_blocks) =
                BlockWatcher::fetch_receipts(&receipt_provider, &landed_blocks).await;
            let receipts_by_hash: HashMap<TxHash, _> =
                receipts.into_iter().map(|receipt| (receipt.tx_hash, receipt)).collect();
            self.collector.apply_receipts(&receipts_by_hash, landed_blocks.len(), failed_blocks);
            info!(
                blocks = landed_blocks.len(),
                failed_blocks,
                receipts = receipts_by_hash.len(),
                elapsed_secs = receipt_fetch_start.elapsed().as_secs_f64(),
                "end-of-run receipt pass complete"
            );
        }

        let summary = self.collector.summarize_with_fresh_recipient_count(
            last_confirmed_at,
            self.config_summary.clone(),
            self.fresh_recipient_count(),
        );
        if let Some(fresh_recipient_count) = summary.fresh_recipient_count {
            info!(fresh_recipient_count, "fresh recipient generation complete");
        }

        if let Some(err) = open_loop_enqueue_error {
            return Err(err);
        }
        Ok(summary)
    }

    async fn open_loop_sender_start_nonces(
        &self,
        sender_addresses: &[Address],
    ) -> Result<Vec<u64>> {
        let nonce_futures = sender_addresses.iter().map(|from| async move {
            let nonce_manager = self.nonce_managers.get(from).ok_or_else(|| {
                BaselineError::Transaction(format!("missing nonce manager for sender {from}"))
            })?;

            nonce_manager.reset().await;
            let nonce_guard = nonce_manager.next_nonce().await.map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to fetch starting nonce for sender {from}: {e}"
                ))
            })?;
            let nonce = nonce_guard.nonce();
            nonce_guard.rollback();
            Ok(nonce)
        });

        stream::iter(nonce_futures).buffered(FUNDING_CONCURRENCY).try_collect().await
    }

    fn select_open_loop_recipient(
        recipient_keys: &mut Option<KeyStream>,
        recipient_rng: &mut SeededRng,
        fresh_recipient_ratio: f64,
        sender_pool_recipient: Address,
    ) -> Result<Address> {
        let Some(recipient_keys) = recipient_keys.as_mut() else {
            return Ok(sender_pool_recipient);
        };

        if fresh_recipient_ratio >= 1.0 || recipient_rng.random::<f64>() < fresh_recipient_ratio {
            Ok(recipient_keys.next_signer()?.address())
        } else {
            Ok(sender_pool_recipient)
        }
    }

    fn build_open_loop_sender_jobs(
        generator: &mut WorkloadGenerator,
        recipient_keys: &mut Option<KeyStream>,
        recipient_rng: &mut SeededRng,
        fresh_recipient_ratio: f64,
        sender_addresses: &[Address],
        sender_start_nonces: &[u64],
        txs_per_sender: usize,
    ) -> Result<Vec<OpenLoopSenderJob>> {
        if sender_addresses.len() != sender_start_nonces.len() {
            return Err(BaselineError::Transaction(format!(
                "open-loop sender nonce set mismatch: {} addresses vs {} nonces",
                sender_addresses.len(),
                sender_start_nonces.len(),
            )));
        }

        let sender_count = sender_addresses.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }
        let mut sender_jobs = Vec::with_capacity(sender_count);
        for (sender_index, from) in sender_addresses.iter().copied().enumerate() {
            let sender_pool_recipient = sender_addresses[(sender_index + 1) % sender_count];
            let mut prepared_txs = Vec::with_capacity(txs_per_sender);
            for _ in 0..txs_per_sender {
                let payload = generator.select_payload()?;
                let to = if payload.uses_runner_recipient() {
                    Self::select_open_loop_recipient(
                        recipient_keys,
                        recipient_rng,
                        fresh_recipient_ratio,
                        sender_pool_recipient,
                    )?
                } else {
                    sender_pool_recipient
                };

                let tx_request = generator.generate_selected_payload(&payload, from, to);
                let to_addr = tx_request.to.and_then(|kind| kind.to().copied());
                let value = tx_request.value.unwrap_or(U256::ZERO);
                let data = tx_request.input.input().cloned().unwrap_or_default();
                let gas_limit = tx_request.gas.unwrap_or(21_000);

                prepared_txs.push(PreparedTransaction {
                    from,
                    to: to_addr,
                    value,
                    data,
                    gas_limit,
                });
            }

            sender_jobs.push(OpenLoopSenderJob {
                sender_index,
                from,
                start_nonce: sender_start_nonces[sender_index],
                prepared_txs,
            });
        }

        Ok(sender_jobs)
    }

    async fn sign_open_loop_sender_jobs(
        sender_jobs: Vec<OpenLoopSenderJob>,
        signers: Arc<HashMap<Address, PrivateKeySigner>>,
        chain_id: u64,
        base_fee: u128,
        max_gas_price: u128,
    ) -> Result<Vec<Vec<SignedTransaction>>> {
        let sender_count = sender_jobs.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }

        let priority_fee = (base_fee / 10).max(1);
        let max_fee = SubmissionPipeline::submission_max_fee(base_fee, priority_fee, max_gas_price);

        let mut signing_tasks = Vec::with_capacity(sender_count);
        for sender_job in sender_jobs {
            let Some(signer) = signers.get(&sender_job.from).cloned() else {
                return Err(BaselineError::Transaction(format!(
                    "missing signer for sender {}",
                    sender_job.from
                )));
            };
            signing_tasks.push(task::spawn_blocking(move || {
                Self::sign_open_loop_sender_job(sender_job, signer, chain_id, priority_fee, max_fee)
            }));
        }

        let mut signed_by_sender: Vec<Option<Vec<SignedTransaction>>> =
            std::iter::repeat_with(|| None).take(sender_count).collect();

        for signing_task in signing_tasks {
            let signed_sender = signing_task.await.map_err(|e| {
                BaselineError::Transaction(format!("open-loop signing task failed: {e}"))
            })??;

            let sender_index = signed_sender.sender_index;
            if signed_by_sender[sender_index].is_some() {
                return Err(BaselineError::Transaction(format!(
                    "duplicate signed sender result for index {sender_index}"
                )));
            }
            signed_by_sender[sender_index] = Some(signed_sender.signed_txs);
        }

        let mut ordered_signed_txs = Vec::with_capacity(sender_count);
        for (sender_index, sender_txs) in signed_by_sender.into_iter().enumerate() {
            let sender_txs = sender_txs.ok_or_else(|| {
                BaselineError::Transaction(format!(
                    "missing signed transaction set for sender index {sender_index}"
                ))
            })?;
            ordered_signed_txs.push(sender_txs);
        }

        Ok(ordered_signed_txs)
    }

    async fn stream_open_loop_presigned_transactions(
        mut producer_state: OpenLoopPresignProducerState,
        mut config: OpenLoopPresignConfig,
    ) -> Result<OpenLoopPresignProducerState> {
        if config.sender_addresses.is_empty() {
            return Ok(producer_state);
        }

        let sender_count = config.sender_addresses.len();
        let chunk_per_sender = (OPEN_LOOP_SIGNED_BATCH_SIZE / sender_count).max(1);
        let mut chunk_index = 0usize;

        // The producer streams indefinitely and only stops when the consumer drops
        // `signed_chunk_rx` (detected below via the `send(...).is_err()` check), which
        // happens once the enqueue loop returns (deadline reached or channel closed).
        loop {
            let sender_jobs = Self::build_open_loop_sender_jobs(
                &mut producer_state.generator,
                &mut producer_state.recipient_keys,
                &mut producer_state.recipient_rng,
                config.fresh_recipient_ratio,
                &config.sender_addresses,
                &config.sender_next_nonces,
                chunk_per_sender,
            )?;

            let base_fee = *config.base_fee_rx.borrow_and_update();
            let signed_by_sender = Self::sign_open_loop_sender_jobs(
                sender_jobs,
                Arc::clone(&config.signers),
                config.chain_id,
                base_fee,
                config.max_gas_price,
            )
            .await?;

            if config.signed_chunk_tx.send(signed_by_sender).await.is_err() {
                break;
            }

            let nonce_increment = u64::try_from(chunk_per_sender).map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to convert open-loop chunk size to nonce increment: {e}"
                ))
            })?;
            for (sender_index, next_nonce) in config.sender_next_nonces.iter_mut().enumerate() {
                *next_nonce = next_nonce.checked_add(nonce_increment).ok_or_else(|| {
                    BaselineError::Transaction(format!(
                        "nonce overflow while advancing open-loop producer sender index {sender_index}"
                    ))
                })?;
            }

            chunk_index = chunk_index.saturating_add(1);
            debug!(chunk_index, chunk_per_sender, "open-loop pre-sign producer generated chunk");
        }

        Ok(producer_state)
    }

    fn sign_open_loop_sender_job(
        sender_job: OpenLoopSenderJob,
        signer: PrivateKeySigner,
        chain_id: u64,
        priority_fee: u128,
        max_fee: u128,
    ) -> Result<OpenLoopSignedSender> {
        let mut signed_txs = Vec::with_capacity(sender_job.prepared_txs.len());

        for (nonce_offset, prepared) in sender_job.prepared_txs.into_iter().enumerate() {
            let nonce_offset = u64::try_from(nonce_offset).map_err(|e| {
                BaselineError::Transaction(format!("failed to convert nonce offset to u64: {e}"))
            })?;
            let nonce = sender_job.start_nonce.checked_add(nonce_offset).ok_or_else(|| {
                BaselineError::Transaction(format!(
                    "nonce overflow for sender {} at offset {nonce_offset}",
                    sender_job.from
                ))
            })?;

            let mut tx = TransactionRequest::default()
                .with_from(prepared.from)
                .with_value(prepared.value)
                .with_input(prepared.data)
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority_fee)
                .with_gas_limit(prepared.gas_limit);
            if let Some(to) = prepared.to {
                tx = tx.with_to(to);
            }

            let typed_tx = tx.build_typed_tx().map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to build typed tx for sender {} nonce {}: {e:?}",
                    prepared.from, nonce
                ))
            })?;

            let sig_hash = typed_tx.signature_hash();
            let signature = signer.sign_hash_sync(&sig_hash).map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to sign tx for sender {} nonce {}: {e}",
                    prepared.from, nonce
                ))
            })?;

            let signed = typed_tx.into_signed(signature);
            let tx_hash = *signed.hash();
            let raw = Bytes::from(signed.encoded_2718());

            signed_txs.push(SignedTransaction { raw, tx_hash, from: prepared.from, nonce });
        }

        Ok(OpenLoopSignedSender { sender_index: sender_job.sender_index, signed_txs })
    }

    async fn enqueue_signed_while_draining(
        submission_pipeline: &SubmissionPipeline,
        batch: SignedBatch,
        drain_state: &mut OpenLoopDrainState<'_>,
    ) -> std::result::Result<(), SignedBatch> {
        let enqueue = submission_pipeline.enqueue_signed(batch);
        tokio::pin!(enqueue);

        loop {
            tokio::select! {
                result = enqueue.as_mut() => {
                    drain_state.drain_run_events();
                    return result;
                }
                maybe_event = drain_state.submit_event_rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            drain_state.apply_submit_event(event);
                            drain_state.drain_run_events();
                        }
                        None => return enqueue.as_mut().await,
                    }
                }
            }
        }
    }

    /// Blocks new submissions while outstanding work (submitted-but-unconfirmed
    /// `total_in_flight`, plus handed-to-the-pipeline-but-not-yet-RPC-accepted
    /// `queued_per_sender`) is at or above `target_in_flight`, draining confirmation
    /// and submit events in the meantime.
    ///
    /// This paces the open-loop fill to the builder's drain rate so the pool holds a
    /// steady depth instead of being flooded in one burst and then draining empty.
    /// A `target_in_flight` of 0 disables pacing (unbounded fill).
    ///
    /// Outstanding work is released by the block watcher mutating the shared
    /// `ResultsTracker` out-of-band, not by submit events, so the wait re-reads
    /// `total_outstanding` on a fixed interval rather than only when a submit event
    /// arrives; a `recv` timeout means "re-check", never "grant headroom". If
    /// outstanding work fails to fall for `OPEN_LOOP_HEADROOM_STALL_TIMEOUT` the
    /// confirmation path is dead and the gate fails closed (returns `Err`) instead of
    /// hanging or flooding the pool.
    async fn wait_for_outstanding_headroom(
        target_in_flight: u64,
        deadline: Option<Instant>,
        stop_when_accepted_target_reached: bool,
        stop_flag: &AtomicBool,
        drain_state: &mut OpenLoopDrainState<'_>,
    ) -> Result<()> {
        if target_in_flight == 0 {
            return Ok(());
        }

        let mut last_outstanding = drain_state.total_outstanding();
        let mut last_progress = Instant::now();

        while drain_state.total_outstanding() >= target_in_flight {
            if stop_flag.load(Ordering::SeqCst) {
                return Err(BaselineError::Transaction("stopped during open-loop enqueue".into()));
            }
            if stop_when_accepted_target_reached
                && drain_state.results_tracker.total_in_flight() >= target_in_flight
            {
                return Ok(());
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                if stop_when_accepted_target_reached {
                    return Err(BaselineError::Timeout {
                        operation: "open-loop mempool prefill".into(),
                        duration: OPEN_LOOP_PREFILL_TIMEOUT,
                    });
                }
                return Ok(());
            }

            match tokio::time::timeout(
                OPEN_LOOP_HEADROOM_RECHECK_INTERVAL,
                drain_state.submit_event_rx.recv(),
            )
            .await
            {
                Ok(Some(event)) => {
                    drain_state.apply_submit_event(event);
                    drain_state.drain_run_events();
                }
                Ok(None) => return Ok(()),
                Err(_) => drain_state.drain_run_events(),
            }

            let current = drain_state.total_outstanding();
            if current < last_outstanding {
                last_outstanding = current;
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= OPEN_LOOP_HEADROOM_STALL_TIMEOUT {
                return Err(BaselineError::Timeout {
                    operation: format!(
                        "open-loop outstanding headroom (stuck at {current} outstanding, target {target_in_flight})"
                    ),
                    duration: OPEN_LOOP_HEADROOM_STALL_TIMEOUT,
                });
            }
        }

        Ok(())
    }

    async fn enqueue_open_loop_signed_transactions(
        submission_pipeline: &SubmissionPipeline,
        next_submit_batch_id: &AtomicU64,
        signed_chunk_rx: &mut mpsc::Receiver<Vec<Vec<SignedTransaction>>>,
        progress: &mut OpenLoopEnqueueProgress,
        deadline: Option<Instant>,
        stop_when_accepted_target_reached: bool,
        stop_flag: &AtomicBool,
        drain_state: &mut OpenLoopDrainState<'_>,
    ) -> Result<()> {
        let mut pending_signed_batch = Vec::with_capacity(OPEN_LOOP_SIGNED_BATCH_SIZE);
        let mut setup_target_reached = false;

        loop {
            drain_state.drain_run_events();
            if stop_flag.load(Ordering::SeqCst) {
                return Err(BaselineError::Transaction("stopped during open-loop enqueue".into()));
            }
            if stop_when_accepted_target_reached
                && drain_state.results_tracker.total_in_flight()
                    >= progress.headroom_target.current_target_in_flight()
            {
                if !pending_signed_batch.is_empty()
                    && !Self::enqueue_open_loop_signed_batch(
                        submission_pipeline,
                        next_submit_batch_id,
                        &mut pending_signed_batch,
                        false,
                        drain_state,
                    )
                    .await
                {
                    return Err(BaselineError::Transaction(
                        "submit queue closed while flushing setup nonce range".into(),
                    ));
                }
                return Ok(());
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                if stop_when_accepted_target_reached {
                    return Err(BaselineError::Timeout {
                        operation: "open-loop mempool prefill".into(),
                        duration: OPEN_LOOP_PREFILL_TIMEOUT,
                    });
                }
                return Ok(());
            }

            let signed_by_sender = match tokio::time::timeout(
                OPEN_LOOP_HEADROOM_RECHECK_INTERVAL,
                signed_chunk_rx.recv(),
            )
            .await
            {
                Ok(Some(signed_by_sender)) => signed_by_sender,
                Ok(None) => break,
                Err(_) => continue,
            };

            for sender_signed in &signed_by_sender {
                progress.presigned_generated =
                    progress.presigned_generated.saturating_add(sender_signed.len() as u64);
            }

            let mut sender_iters =
                signed_by_sender.into_iter().map(Vec::into_iter).collect::<Vec<_>>();

            loop {
                let mut made_progress = false;

                for sender_iter in &mut sender_iters {
                    if let Some(signed_tx) = sender_iter.next() {
                        made_progress = true;
                        pending_signed_batch.push(signed_tx);

                        if pending_signed_batch.len() >= OPEN_LOOP_SIGNED_BATCH_SIZE {
                            if !setup_target_reached {
                                if let Some(update) = progress.headroom_target.maybe_update(
                                    Instant::now(),
                                    drain_state.collector.confirmed_count() as u64,
                                    drain_state.results_tracker.observed_avg_gas(),
                                ) {
                                    debug!(
                                        previous_target_in_flight =
                                            update.previous_target_in_flight,
                                        updated_target_in_flight = update.updated_target_in_flight,
                                        confirmed_delta = update.confirmed_delta,
                                        sample_tps = update.sample_tps,
                                        smoothed_tps = update.smoothed_tps,
                                        "adjusted open-loop in-flight target"
                                    );
                                }
                                Self::wait_for_outstanding_headroom(
                                    progress.headroom_target.current_target_in_flight(),
                                    deadline,
                                    stop_when_accepted_target_reached,
                                    stop_flag,
                                    drain_state,
                                )
                                .await?;
                                setup_target_reached = stop_when_accepted_target_reached
                                    && drain_state.results_tracker.total_in_flight()
                                        >= progress.headroom_target.current_target_in_flight();
                                if !setup_target_reached
                                    && deadline.is_some_and(|d| Instant::now() >= d)
                                {
                                    if stop_when_accepted_target_reached {
                                        return Err(BaselineError::Timeout {
                                            operation: "open-loop mempool prefill".into(),
                                            duration: OPEN_LOOP_PREFILL_TIMEOUT,
                                        });
                                    }
                                    return Ok(());
                                }
                            }
                            if !Self::enqueue_open_loop_signed_batch(
                                submission_pipeline,
                                next_submit_batch_id,
                                &mut pending_signed_batch,
                                !stop_when_accepted_target_reached,
                                drain_state,
                            )
                            .await
                            {
                                if stop_when_accepted_target_reached {
                                    return Err(BaselineError::Transaction(
                                        "submit queue closed during setup prefill".into(),
                                    ));
                                }
                                return Ok(());
                            }
                        }
                    }
                }

                if !made_progress {
                    break;
                }
            }

            if setup_target_reached {
                if !pending_signed_batch.is_empty()
                    && !Self::enqueue_open_loop_signed_batch(
                        submission_pipeline,
                        next_submit_batch_id,
                        &mut pending_signed_batch,
                        false,
                        drain_state,
                    )
                    .await
                {
                    return Err(BaselineError::Transaction(
                        "submit queue closed while flushing setup nonce range".into(),
                    ));
                }
                return Ok(());
            }

            drain_state.drain_run_events();
        }

        if !pending_signed_batch.is_empty() {
            if let Some(update) = progress.headroom_target.maybe_update(
                Instant::now(),
                drain_state.collector.confirmed_count() as u64,
                drain_state.results_tracker.observed_avg_gas(),
            ) {
                debug!(
                    previous_target_in_flight = update.previous_target_in_flight,
                    updated_target_in_flight = update.updated_target_in_flight,
                    confirmed_delta = update.confirmed_delta,
                    sample_tps = update.sample_tps,
                    smoothed_tps = update.smoothed_tps,
                    "adjusted open-loop in-flight target"
                );
            }
            Self::wait_for_outstanding_headroom(
                progress.headroom_target.current_target_in_flight(),
                deadline,
                stop_when_accepted_target_reached,
                stop_flag,
                drain_state,
            )
            .await?;
            let enqueued = Self::enqueue_open_loop_signed_batch(
                submission_pipeline,
                next_submit_batch_id,
                &mut pending_signed_batch,
                !stop_when_accepted_target_reached,
                drain_state,
            )
            .await;
            if !enqueued && stop_when_accepted_target_reached {
                return Err(BaselineError::Transaction(
                    "submit queue closed while flushing setup nonce range".into(),
                ));
            }
        }

        Ok(())
    }

    async fn enqueue_open_loop_signed_batch(
        submission_pipeline: &SubmissionPipeline,
        next_submit_batch_id: &AtomicU64,
        pending_signed_batch: &mut Vec<SignedTransaction>,
        measured: bool,
        drain_state: &mut OpenLoopDrainState<'_>,
    ) -> bool {
        let signed_txs = std::mem::replace(
            pending_signed_batch,
            Vec::with_capacity(OPEN_LOOP_SIGNED_BATCH_SIZE),
        );
        let batch_len = signed_txs.len();
        let batch_id = next_submit_batch_id.fetch_add(1, Ordering::SeqCst);

        for signed_tx in &signed_txs {
            drain_state
                .queued_per_sender
                .entry(signed_tx.from)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }

        let batch = SignedBatch { id: batch_id, attempt: 0, measured, txs: signed_txs };
        match Self::enqueue_signed_while_draining(submission_pipeline, batch, drain_state).await {
            Ok(()) => {
                debug!(batch_id, batch_len, "queued open-loop signed batch");
                true
            }
            Err(batch) => {
                warn!(
                    batch_id,
                    batch_len, "submit queue closed while enqueuing open-loop signed batch"
                );
                let failed_count = batch.txs.len() as u64;
                for signed_tx in &batch.txs {
                    if let Some(count) = drain_state.queued_per_sender.get_mut(&signed_tx.from) {
                        *count = count.saturating_sub(1);
                    }
                }
                drain_state.collector.record_failures("submit queue closed", failed_count);
                false
            }
        }
    }

    fn build_snapshot(
        &mut self,
        start: Instant,
        results_tracker: &ResultsTracker,
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
            in_flight: results_tracker.total_in_flight(),
            senders_blocked: results_tracker.senders_at_limit(max_in_flight_per_sender),
            total_senders: account_count,
            rolling_tps: self.collector.rolling_tps(),
            rolling_gps: self.collector.rolling_gps(),
            p50_latency: p50,
            p99_latency: p99,
            flashblocks_p50_latency: flashblocks_p50,
            flashblocks_p99_latency: flashblocks_p99,
            gas_price_gwei: self.base_fee as f64 / 1e9,
            total_eth: self.last_total_eth.clone(),
            min_eth: self.last_min_eth.clone(),
            funds_low: self.last_funds_low,
            funder_address: self.funder_address.clone(),
            sender_addresses: self.sender_addresses.clone(),
        }
    }

    fn apply_submit_event(
        event: SubmitEvent,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
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

    fn drain_submit_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            Self::apply_submit_event(event, queued_per_sender, collector);
        }
    }

    fn drain_run_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
        results_tracker: &ResultsTracker,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            Self::apply_submit_event(event, queued_per_sender, collector);
        }
        for metrics in results_tracker.drain_confirmed_metrics() {
            collector.record_confirmed(metrics);
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

        let base_fee = client.get_base_fee().await?;
        let max_priority_fee = (base_fee / 10).max(1);
        let max_fee = SubmissionPipeline::submission_max_fee(
            base_fee,
            max_priority_fee,
            self.config.max_gas_price,
        );
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

    pub(super) fn progress_bar(&self, total: u64, prefix: &str) -> ProgressBar {
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
    pub(super) async fn await_token_balances(
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

    pub(super) async fn refresh_sender_state(&mut self) -> Result<()> {
        let total_accounts = self.accounts.len();
        let client = self.client.clone();
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

        let refresh_provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());

        for result in refresh_results {
            let (addr, balance, account_nonce) = result?;
            let idx = addr_to_idx[&addr];
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = account_nonce;

            let nonce_manager =
                NonceManager::new(refresh_provider.clone(), addr, NONCE_RPC_TIMEOUT)
                    .with_pending_tag();
            Arc::make_mut(&mut self.nonce_managers).insert(addr, nonce_manager);

            debug!(address = %addr, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        Ok(())
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

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, Bytes, TxHash};
    use tokio::sync::mpsc;

    use super::{
        LoadConfig, LoadRunner, MetricsCollector, OPEN_LOOP_SIGNED_BATCH_SIZE,
        OPEN_LOOP_TARGET_LOOKAHEAD_SECONDS, OPEN_LOOP_TARGET_MARGIN_MULTIPLIER,
        OPEN_LOOP_TARGET_UPDATE_INTERVAL, OpenLoopDrainState, OpenLoopHeadroomTarget,
        PipelineStartConfig, ResultsTracker, SignedBatch, SignedTransaction, SubmissionPipeline,
        SubmitEvent,
    };
    use crate::runner::SUBMIT_BATCH_QUEUE_BUFFER;

    #[test]
    fn mempool_target_uses_ceiling_and_checks_capacity() {
        assert_eq!(LoadRunner::mempool_target_transactions(100, 3, 70, 10).unwrap(), 5);
        assert!(LoadRunner::mempool_target_transactions(100, 3, 70, 4).is_err());
        assert!(LoadRunner::mempool_target_transactions(100, 3, 0, 10).is_err());
    }

    fn test_signed_batch(id: u64, from: Address) -> SignedBatch {
        SignedBatch {
            id,
            attempt: 0,
            measured: true,
            txs: vec![SignedTransaction {
                raw: Bytes::new(),
                tx_hash: TxHash::repeat_byte((id % 0xff) as u8),
                from,
                nonce: id,
            }],
        }
    }

    #[tokio::test]
    async fn enqueue_signed_while_draining_makes_progress_when_enqueue_is_backpressured() {
        let sender = Address::repeat_byte(0x11);
        let tracked_senders = vec![sender];
        let results_tracker = ResultsTracker::new(&tracked_senders);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::from([(sender, 1_u64)]);

        let (submit_event_tx, mut submit_event_rx) = mpsc::channel(1);
        let mut submission_pipeline = SubmissionPipeline::start(
            Arc::new(HashMap::new()),
            Arc::new(HashMap::new()),
            Arc::new(Vec::new()),
            results_tracker.clone(),
            submit_event_tx.clone(),
            PipelineStartConfig { chain_id: 1, max_gas_price: u128::MAX },
        );

        submission_pipeline.shutdown_and_join(Duration::from_secs(1)).await;

        for batch_id in 0..SUBMIT_BATCH_QUEUE_BUFFER {
            submission_pipeline
                .enqueue_signed(test_signed_batch(batch_id as u64, sender))
                .await
                .expect("signed queue should accept up to capacity");
        }

        submit_event_tx
            .send(SubmitEvent::Released(sender))
            .await
            .expect("event queue should accept first event");

        let release_after_event_drain = async {
            submit_event_tx
                .send(SubmitEvent::Submitted(TxHash::repeat_byte(0xaa)))
                .await
                .expect("second send should unblock only after event drain");
            let _ = submission_pipeline.close_and_fail_queued("test queue close").await;
        };

        let (enqueue_result, ()) = {
            let mut drain_state = OpenLoopDrainState {
                submit_event_rx: &mut submit_event_rx,
                queued_per_sender: &mut queued_per_sender,
                collector: &mut collector,
                results_tracker: &results_tracker,
            };
            let enqueue_attempt = LoadRunner::enqueue_signed_while_draining(
                &submission_pipeline,
                test_signed_batch((SUBMIT_BATCH_QUEUE_BUFFER + 1) as u64, sender),
                &mut drain_state,
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(enqueue_attempt, release_after_event_drain)
            })
            .await
            .expect("enqueue should complete once event drain unblocks it")
        };

        assert!(enqueue_result.is_err(), "enqueue should fail after queue is closed");
        assert_eq!(queued_per_sender.get(&sender).copied(), Some(0));
        assert_eq!(collector.submitted_count(), 1);
    }

    #[tokio::test]
    async fn headroom_gate_resumes_when_block_watcher_lowers_in_flight_without_submit_event() {
        use std::time::Instant;

        use super::super::results_tracker::{BlockObservation, SentTransaction};

        let sender = Address::repeat_byte(0x22);
        let results_tracker = ResultsTracker::new(&[sender]);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::new();
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel::<SubmitEvent>(1);

        let tx_hash = TxHash::repeat_byte(0x22);
        results_tracker.sent_transactions(vec![SentTransaction {
            tx_hash,
            from: sender,
            measured: true,
        }]);
        assert_eq!(results_tracker.total_in_flight(), 1);

        let tracker_for_watcher = results_tracker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            tracker_for_watcher.on_new_block_hashes(
                BlockObservation { number: 1, observed_at: Instant::now() },
                vec![tx_hash],
            );
        });

        let mut drain_state = OpenLoopDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            collector: &mut collector,
            results_tracker: &results_tracker,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            LoadRunner::wait_for_outstanding_headroom(
                1,
                None,
                false,
                &AtomicBool::new(false),
                &mut drain_state,
            ),
        )
        .await
        .expect("gate must not hang when in-flight is released out-of-band");

        assert!(result.is_ok(), "gate should resume once in-flight drops below target");
        assert_eq!(results_tracker.total_in_flight(), 0);
    }

    #[tokio::test]
    async fn headroom_gate_returns_immediately_when_pacing_disabled() {
        use super::super::results_tracker::SentTransaction;

        let sender = Address::repeat_byte(0x44);
        let results_tracker = ResultsTracker::new(&[sender]);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::new();
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel::<SubmitEvent>(1);

        results_tracker.sent_transactions(vec![SentTransaction {
            tx_hash: TxHash::repeat_byte(0x44),
            from: sender,
            measured: true,
        }]);

        let mut drain_state = OpenLoopDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            collector: &mut collector,
            results_tracker: &results_tracker,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            LoadRunner::wait_for_outstanding_headroom(
                0,
                None,
                false,
                &AtomicBool::new(false),
                &mut drain_state,
            ),
        )
        .await
        .expect("target 0 must return without waiting even while in-flight is high");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn headroom_gate_returns_ok_when_past_deadline() {
        use std::time::Instant;

        use super::super::results_tracker::SentTransaction;

        let sender = Address::repeat_byte(0x55);
        let results_tracker = ResultsTracker::new(&[sender]);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::new();
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel::<SubmitEvent>(1);

        results_tracker.sent_transactions(vec![SentTransaction {
            tx_hash: TxHash::repeat_byte(0x55),
            from: sender,
            measured: true,
        }]);

        let mut drain_state = OpenLoopDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            collector: &mut collector,
            results_tracker: &results_tracker,
        };

        let past_deadline = Some(Instant::now() - Duration::from_secs(1));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            LoadRunner::wait_for_outstanding_headroom(
                1,
                past_deadline,
                false,
                &AtomicBool::new(false),
                &mut drain_state,
            ),
        )
        .await
        .expect("gate must stop waiting once the load window deadline has passed");
        assert!(result.is_ok(), "past-deadline gate returns Ok without failing closed");
        assert_eq!(
            results_tracker.total_in_flight(),
            1,
            "in-flight stays high; gate returns because window ended, not because it drained"
        );
    }

    #[test]
    fn open_loop_headroom_target_clamps_to_target_gps_cap() {
        let now = Instant::now();
        let sampled_at = now - OPEN_LOOP_TARGET_UPDATE_INTERVAL;
        let mut target =
            OpenLoopHeadroomTarget::new(10_000, Some(2_100_000), 21_000, 0, sampled_at);

        let update = target
            .maybe_update(now, 1_000, None)
            .expect("elapsed update interval should produce a new target");

        // Old behavior (without a gas-rate cap) would produce this larger target:
        // sample_tps=500 over 2s, then EWMA/margin/lookahead plus one batch buffer.
        let unclamped_target =
            ((500.0 * OPEN_LOOP_TARGET_MARGIN_MULTIPLIER * OPEN_LOOP_TARGET_LOOKAHEAD_SECONDS)
                .ceil() as u64)
                .saturating_add(OPEN_LOOP_SIGNED_BATCH_SIZE as u64);
        assert!(
            unclamped_target > 100,
            "test setup must produce a cap effect; unclamped target={unclamped_target}"
        );
        assert_eq!(update.updated_target_in_flight, 100);
        assert_eq!(target.current_target_in_flight(), 100);
    }

    #[test]
    fn saturated_headroom_target_tracks_observed_average_gas() {
        let now = Instant::now();
        let mut target = OpenLoopHeadroomTarget::saturated(
            300_000,
            3,
            10,
            100_000,
            now - OPEN_LOOP_TARGET_UPDATE_INTERVAL,
        );

        let update = target
            .maybe_update(now, 1, Some(60_000))
            .expect("elapsed update interval should recalibrate the inventory");

        assert_eq!(update.updated_target_in_flight, 5);
        assert_eq!(target.current_target_in_flight(), 5);
    }

    #[test]
    fn open_loop_headroom_target_without_cap_matches_previous_formula() {
        let now = Instant::now();
        let sampled_at = now - OPEN_LOOP_TARGET_UPDATE_INTERVAL;
        let mut target = OpenLoopHeadroomTarget::new(10_000, None, 21_000, 0, sampled_at);

        let update = target
            .maybe_update(now, 1_000, Some(21_000))
            .expect("elapsed update interval should produce a new target");

        let expected =
            ((500.0 * OPEN_LOOP_TARGET_MARGIN_MULTIPLIER * OPEN_LOOP_TARGET_LOOKAHEAD_SECONDS)
                .ceil() as u64)
                .saturating_add(OPEN_LOOP_SIGNED_BATCH_SIZE as u64)
                .clamp(OPEN_LOOP_SIGNED_BATCH_SIZE as u64, 10_000);
        assert_eq!(update.updated_target_in_flight, expected);
        assert_eq!(target.current_target_in_flight(), expected);
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
