//! Validity-transaction flood generator.
//!
//! Reproduces the mainnet validity traffic pattern behind the October/November 2026
//! incidents: many senders each hold a single pending nonce and keep re-quoting it
//! through `base_sendRawTransactionValidity` with predicates that rarely or never pass.
//! The transactions rest in the builder's validity pool until their block-number
//! expiry and cost the sender nothing unless they land.
//!
//! Each sender runs in windows. A window starts with an add at `current_block +
//! lifetime_blocks` expiry, followed by up to `max_requotes_per_window` same-nonce
//! replacements, each with fees bumped by `fee_bump_percent`. After the expiry passes,
//! the next window adds again at base fees. When predicates pass and a transaction
//! lands, the next submission fails with `nonce too low`; the sender then reloads its
//! nonce and starts a new window.
//!
//! The flood measures nothing on chain. Run it next to an ordinary load test that
//! measures plain-transaction inclusion, and read builder metrics in Datadog.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_PREDICATES, ValidityAuthorization, ValidityOperator, ValidityPredicate,
};
use futures::{StreamExt, stream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    AccountPool, BaseFeeExt, BatchRpcClient, BatchSendResult, QueryProvider, RpcProviders,
    SubmitItem, ValidityPredicateConfig, ValidityPredicateTemplate, ValidityRouter,
    utils::{BaselineError, Result},
};

/// Predicate cap enforced by production mempool and builder ingress (`--validity-max-predicates`).
pub const PRODUCTION_MAX_VALIDITY_PREDICATES: usize = 16;

const FUNDING_GAS_LIMIT: u64 = 21_000;
const FUNDING_CHUNK: usize = 100;
const FUNDING_ROUND: usize = 1_000;
const FUNDING_TIMEOUT: Duration = Duration::from_secs(180);
const RPC_READ_CONCURRENCY: usize = 32;
const RPC_READ_ATTEMPTS: u32 = 6;
const HEAD_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BASE_FEE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// One phase of a flood. Stages run in order; use several to ramp up load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloodStage {
    /// Stage length, e.g. `"2m"`.
    pub duration: String,
    /// Number of senders submitting during this stage; the first N configured senders.
    pub active_senders: u32,
    /// How often each active sender submits (an add or a replacement), e.g. `"500ms"`.
    pub requote_interval: String,
}

/// YAML configuration for `base-load-tester validity-flood`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidityFloodConfig {
    /// Endpoints serving `base_sendRawTransactionValidity`; workers spread across them.
    pub submission_rpcs: Vec<Url>,
    /// Endpoint for block number, base fee, balance and nonce reads.
    pub query_rpc: Url,
    /// Chain ID; read from `query_rpc` when omitted.
    #[serde(default)]
    pub chain_id: Option<u64>,
    /// Seed for deterministic sender keys. Use a seed or offset distinct from any
    /// concurrently running load test.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Number of derived keys to skip before the first sender.
    #[serde(default)]
    pub sender_offset: u32,
    /// Number of flood senders to derive and fund.
    pub sender_count: u32,
    /// Wei to top each sender up to. Resting transactions pay nothing; this covers
    /// the occasional landed transaction and the balance check at admission.
    #[serde(default = "default_funding_amount")]
    pub funding_amount: U256,
    /// Skip the funding step, e.g. when re-running with funded senders.
    #[serde(default)]
    pub skip_funding: bool,
    /// Flood phases, run in order.
    pub stages: Vec<FloodStage>,
    /// Expiry: each window's transactions carry `block_number <= current + lifetime_blocks`.
    /// At 200ms blocks, 75 = 15s (mainnet ingress cap today) and 300 = 60s (default).
    #[serde(default = "default_lifetime_blocks")]
    pub lifetime_blocks: u64,
    /// Same-nonce replacements submitted after the window's add, before waiting for expiry.
    #[serde(default = "default_max_requotes")]
    pub max_requotes_per_window: u32,
    /// Fee increase per replacement, in percent. Pools require at least 10.
    #[serde(default = "default_fee_bump_percent")]
    pub fee_bump_percent: u32,
    /// Priority fee for a window's add, in wei.
    #[serde(default = "default_priority_fee")]
    pub priority_fee: u128,
    /// Window add max fee = base fee * this multiplier + priority fee.
    #[serde(default = "default_max_fee_multiplier")]
    pub max_fee_base_fee_multiplier: u128,
    /// Gas limit for each flood transaction.
    #[serde(default = "default_gas_limit")]
    pub gas_limit: u64,
    /// Call target; defaults to a zero-value self-transfer.
    #[serde(default)]
    pub to: Option<Address>,
    /// Calldata length per transaction; increases pool bytes per entry.
    #[serde(default)]
    pub calldata_bytes: usize,
    /// Transactions per JSON-RPC batch request.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Submission workers; each has at most one batch request outstanding.
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// Attach an EIP-712 sender authorization to every sidecar.
    #[serde(default)]
    pub sign_predicates: bool,
    /// Allow more than [`PRODUCTION_MAX_VALIDITY_PREDICATES`] predicates per transaction.
    #[serde(default)]
    pub allow_over_production_cap: bool,
    /// Wake-up storm: when set, every transaction also carries `block_number >= B`,
    /// where `B` is the next multiple of this value after the current block. Everything
    /// submitted since the previous multiple becomes includable in the same block.
    /// Requires `lifetime_blocks` above this value; counts toward the predicate limit.
    #[serde(default)]
    pub release_every_blocks: Option<u64>,
    /// Predicate templates. A block-number expiry is appended automatically, so it
    /// counts toward the per-transaction limit.
    pub predicates: Vec<ValidityPredicateConfig>,
}

const fn default_seed() -> u64 {
    0x0066_6c6f_6f64 // "flood"
}
fn default_funding_amount() -> U256 {
    U256::from(200_000_000_000_000u64) // 0.0002 ETH
}
const fn default_lifetime_blocks() -> u64 {
    75
}
const fn default_max_requotes() -> u32 {
    3
}
const fn default_fee_bump_percent() -> u32 {
    12
}
const fn default_priority_fee() -> u128 {
    1_000_000
}
const fn default_max_fee_multiplier() -> u128 {
    3
}
const fn default_gas_limit() -> u64 {
    50_000
}
const fn default_batch_size() -> usize {
    50
}
const fn default_workers() -> usize {
    64
}

impl ValidityFloodConfig {
    /// Loads and validates a YAML configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| BaselineError::Config(format!("failed to read flood config: {e}")))?;
        let config: Self = serde_yaml::from_str(&raw)
            .map_err(|e| BaselineError::Config(format!("invalid flood config: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Validates the configuration.
    pub fn validate(&self) -> Result<()> {
        let err = |m: &str| Err(BaselineError::Config(m.to_string()));
        if self.submission_rpcs.is_empty() {
            return err("submission_rpcs must not be empty");
        }
        if self.sender_count == 0 {
            return err("sender_count must be positive");
        }
        if self.stages.is_empty() {
            return err("stages must not be empty");
        }
        for stage in &self.stages {
            parse_duration(&stage.duration)?;
            if parse_duration(&stage.requote_interval)?.is_zero() {
                return err("stage requote_interval must be positive");
            }
            if stage.active_senders > self.sender_count {
                return err("stage active_senders exceeds sender_count");
            }
        }
        if self.lifetime_blocks == 0 {
            return err("lifetime_blocks must be positive");
        }
        if self.fee_bump_percent < 10 {
            return err("fee_bump_percent must be at least 10");
        }
        if self.workers == 0 || self.batch_size == 0 {
            return err("workers and batch_size must be positive");
        }
        if let Some(every) = self.release_every_blocks
            && (every == 0 || every >= self.lifetime_blocks)
        {
            return err("release_every_blocks must be positive and below lifetime_blocks");
        }
        let total = self.predicates.len() + 1 + usize::from(self.release_every_blocks.is_some());
        let limit = if self.allow_over_production_cap {
            DEFAULT_MAX_VALIDITY_PREDICATES
        } else {
            PRODUCTION_MAX_VALIDITY_PREDICATES
        };
        if total > limit {
            return Err(BaselineError::Config(format!(
                "{total} predicates per transaction (including the automatic expiry) exceed the limit of {limit}"
            )));
        }
        for predicate in &self.predicates {
            predicate.to_template()?;
        }
        Ok(())
    }

    fn templates(&self) -> Result<Vec<ValidityPredicateTemplate>> {
        self.predicates.iter().map(ValidityPredicateConfig::to_template).collect()
    }

    fn total_duration(&self) -> Result<Duration> {
        self.stages.iter().map(|s| parse_duration(&s.duration)).sum()
    }
}

fn parse_duration(s: &str) -> Result<Duration> {
    humantime::parse_duration(s)
        .map_err(|e| BaselineError::Config(format!("invalid duration '{s}': {e}")))
}

#[derive(Debug, Clone, Copy)]
struct ResolvedStage {
    ends_at: Duration,
    active_senders: usize,
    interval: Duration,
}

/// Counters shared by workers and the reporter.
#[derive(Debug, Default)]
struct Stats {
    submitted: AtomicU64,
    accepted: AtomicU64,
    adds: AtomicU64,
    replacements: AtomicU64,
    nonce_reloads: AtomicU64,
    late_ticks: AtomicU64,
    errors: Mutex<BTreeMap<String, u64>>,
}

impl Stats {
    fn error(&self, category: String, count: u64) {
        *self.errors.lock().entry(category).or_default() += count;
    }
}

/// Final flood summary, written as JSON with `--output`.
#[derive(Debug, Clone, Serialize)]
pub struct FloodSummary {
    /// Wall-clock submission time.
    pub elapsed_secs: f64,
    /// Transactions sent to an endpoint.
    pub submitted: u64,
    /// Transactions an endpoint accepted.
    pub accepted: u64,
    /// Window-opening submissions.
    pub adds: u64,
    /// Same-nonce replacements.
    pub replacements: u64,
    /// Flood transactions that landed (sum of sender nonce increases).
    pub landed: u64,
    /// Worker ticks that overran their requote interval (client-side saturation).
    pub late_ticks: u64,
    /// Rejections by category.
    pub errors: BTreeMap<String, u64>,
    /// First and last observed block numbers.
    pub blocks: (u64, u64),
}

#[derive(Debug)]
struct Window {
    expiry: u64,
    max_fee: u128,
    priority_fee: u128,
    requotes: u32,
}

#[derive(Debug)]
struct Sender {
    index: usize,
    signer: PrivateKeySigner,
    address: Address,
    nonce: u64,
    window: Option<Window>,
    reload_nonce: bool,
    /// First block this sender may open a window, set when it first becomes active. Senders are
    /// phased across one lifetime so their windows, and expiries, do not line up.
    not_before: Option<u64>,
}

/// Spreads sender phases evenly but unordered across `lifetime_blocks`.
const fn sender_phase(index: usize, lifetime_blocks: u64) -> u64 {
    (index as u64).wrapping_mul(2_654_435_761) % lifetime_blocks
}

enum SubmitKind {
    Add,
    Replacement,
}

struct Plan {
    sender: usize,
    kind: SubmitKind,
}

#[derive(Debug, Clone, Copy)]
struct Pricing {
    priority_fee: u128,
    multiplier: u128,
    bump_percent: u128,
    max_requotes: u32,
    lifetime_blocks: u64,
}

impl Sender {
    fn plan(&mut self, block: u64, base_fee: u128, pricing: Pricing) -> Option<SubmitKind> {
        let expired = self.window.as_ref().is_none_or(|w| block >= w.expiry);
        if expired {
            self.window = Some(Window {
                expiry: block + pricing.lifetime_blocks,
                max_fee: base_fee * pricing.multiplier + pricing.priority_fee,
                priority_fee: pricing.priority_fee,
                requotes: 0,
            });
            return Some(SubmitKind::Add);
        }
        let window = self.window.as_mut()?;
        if window.requotes >= pricing.max_requotes {
            return None;
        }
        let bump = |fee: u128| (fee + fee * pricing.bump_percent / 100).max(fee + 1);
        window.max_fee = bump(window.max_fee);
        window.priority_fee = bump(window.priority_fee).min(window.max_fee);
        window.requotes += 1;
        Some(SubmitKind::Replacement)
    }
}

struct Shared {
    config: ValidityFloodConfig,
    templates: Vec<ValidityPredicateTemplate>,
    stages: Vec<ResolvedStage>,
    chain_id: u64,
    block: AtomicU64,
    base_fee: AtomicU64,
    stats: Stats,
    query: QueryProvider,
    clients: Vec<BatchRpcClient>,
    calldata: Bytes,
}

/// Runs a validity flood until its stages finish or `shutdown` fires.
#[derive(Debug, Clone, Copy)]
pub struct ValidityFlood;

impl ValidityFlood {
    /// Funds senders, then floods per the configured stages.
    pub async fn run(
        config: ValidityFloodConfig,
        funder: PrivateKeySigner,
        shutdown: CancellationToken,
    ) -> Result<FloodSummary> {
        let query = RpcProviders::query(config.query_rpc.clone())?;
        let chain_id = match config.chain_id {
            Some(id) => id,
            None => query
                .get_chain_id()
                .await
                .map_err(|e| BaselineError::Rpc(format!("chain id: {e}")))?,
        };
        let pool = AccountPool::with_offset(
            config.seed,
            config.sender_count as usize,
            config.sender_offset as usize,
        )?;
        let clients = config
            .submission_rpcs
            .iter()
            .map(|url| {
                BatchRpcClient::new(url.clone()).map(|c| c.with_batch_size(config.batch_size))
            })
            .collect::<Result<Vec<_>>>()?;

        let base_fee = query.get_base_fee().await?;
        let block = query
            .get_block_number()
            .await
            .map_err(|e| BaselineError::Rpc(format!("block number: {e}")))?;
        info!(chain_id, senders = config.sender_count, block, base_fee, "validity flood starting");

        if !config.skip_funding {
            Self::fund(&config, &query, &clients[0], &funder, chain_id, &pool, base_fee).await?;
        }

        let addresses = pool.addresses();
        let initial_nonces = Self::nonces(&query, &addresses).await?;
        let senders: Vec<Sender> = pool
            .accounts()
            .iter()
            .zip(&initial_nonces)
            .enumerate()
            .map(|(index, (account, nonce))| Sender {
                index,
                signer: account.signer.clone(),
                address: account.address,
                nonce: *nonce,
                window: None,
                reload_nonce: false,
                not_before: None,
            })
            .collect();

        let mut ends_at = Duration::ZERO;
        let stages = config
            .stages
            .iter()
            .map(|s| {
                ends_at += parse_duration(&s.duration)?;
                Ok(ResolvedStage {
                    ends_at,
                    active_senders: s.active_senders as usize,
                    interval: parse_duration(&s.requote_interval)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let total = config.total_duration()?;

        let shared = Arc::new(Shared {
            templates: config.templates()?,
            calldata: Bytes::from(vec![0x42; config.calldata_bytes]),
            config,
            stages,
            chain_id,
            block: AtomicU64::new(block),
            base_fee: AtomicU64::new(base_fee as u64),
            stats: Stats::default(),
            query,
            clients,
        });

        let stop = shutdown.child_token();
        let head = tokio::spawn(Self::watch_head(Arc::clone(&shared), stop.clone()));
        let started = Instant::now();
        let reporter = tokio::spawn(Self::report(Arc::clone(&shared), started, stop.clone()));

        let worker_count = shared.config.workers;
        let mut shards: Vec<Vec<Sender>> = (0..worker_count).map(|_| Vec::new()).collect();
        for sender in senders {
            shards[sender.index % worker_count].push(sender);
        }
        let workers: Vec<_> = shards
            .into_iter()
            .enumerate()
            .map(|(id, shard)| {
                tokio::spawn(Self::worker(Arc::clone(&shared), id, shard, started, stop.clone()))
            })
            .collect();

        tokio::select! {
            () = tokio::time::sleep(total) => {}
            () = shutdown.cancelled() => warn!("flood interrupted"),
        }
        stop.cancel();
        for worker in workers {
            let _ = worker.await;
        }
        let elapsed = started.elapsed();
        let _ = head.await;
        let _ = reporter.await;

        let final_nonces = Self::nonces(&shared.query, &addresses).await?;
        let landed =
            final_nonces.iter().zip(&initial_nonces).map(|(f, i)| f.saturating_sub(*i)).sum();
        let stats = &shared.stats;
        let summary = FloodSummary {
            elapsed_secs: elapsed.as_secs_f64(),
            submitted: stats.submitted.load(Ordering::Relaxed),
            accepted: stats.accepted.load(Ordering::Relaxed),
            adds: stats.adds.load(Ordering::Relaxed),
            replacements: stats.replacements.load(Ordering::Relaxed),
            landed,
            late_ticks: stats.late_ticks.load(Ordering::Relaxed),
            errors: stats.errors.lock().clone(),
            blocks: (block, shared.block.load(Ordering::Relaxed)),
        };
        info!(
            submitted = summary.submitted,
            accepted = summary.accepted,
            landed = summary.landed,
            "validity flood finished"
        );
        Ok(summary)
    }

    async fn nonces(query: &QueryProvider, addresses: &[Address]) -> Result<Vec<u64>> {
        let results: Vec<_> = stream::iter(addresses.iter().copied())
            .map(|address| async move {
                retry_read(|| async { query.get_transaction_count(address).latest().await }).await
            })
            .buffered(RPC_READ_CONCURRENCY)
            .collect()
            .await;
        results
            .into_iter()
            .map(|r| r.map_err(|e| BaselineError::Rpc(format!("nonce read: {e}"))))
            .collect()
    }

    async fn fund(
        config: &ValidityFloodConfig,
        query: &QueryProvider,
        client: &BatchRpcClient,
        funder: &PrivateKeySigner,
        chain_id: u64,
        pool: &AccountPool,
        base_fee: u128,
    ) -> Result<()> {
        let balances: Vec<_> = stream::iter(pool.addresses())
            .map(|address| async move {
                (address, retry_read(|| async { query.get_balance(address).latest().await }).await)
            })
            .buffered(RPC_READ_CONCURRENCY)
            .collect()
            .await;
        let mut needs = Vec::new();
        for (address, balance) in balances {
            let balance = balance.map_err(|e| BaselineError::Rpc(format!("balance read: {e}")))?;
            if balance < config.funding_amount / U256::from(2) {
                needs.push((address, config.funding_amount - balance));
            }
        }
        if needs.is_empty() {
            info!("all flood senders already funded");
            return Ok(());
        }
        let total: U256 = needs.iter().map(|(_, v)| *v).sum();
        let funder_balance = query
            .get_balance(funder.address())
            .latest()
            .await
            .map_err(|e| BaselineError::Rpc(format!("funder balance: {e}")))?;
        if funder_balance < total {
            return Err(BaselineError::Config(format!(
                "funder balance {funder_balance} wei is below the {total} wei needed to fund {} senders",
                needs.len()
            )));
        }
        let priority_fee = default_priority_fee() * 10;
        let max_fee = base_fee * 4 + priority_fee;
        info!(senders = needs.len(), total_wei = %total, "funding flood senders");

        // Fund in rounds that each land before the next, so one funder never holds more
        // pending transactions than a pool's per-sender limits allow.
        for (round, batch) in needs.chunks(FUNDING_ROUND).enumerate() {
            let start_nonce = query
                .get_transaction_count(funder.address())
                .latest()
                .await
                .map_err(|e| BaselineError::Rpc(format!("funder nonce: {e}")))?;
            let mut items = Vec::with_capacity(batch.len());
            for (i, (to, value)) in batch.iter().enumerate() {
                let tx = TxEip1559 {
                    chain_id,
                    nonce: start_nonce + i as u64,
                    gas_limit: FUNDING_GAS_LIMIT,
                    max_fee_per_gas: max_fee,
                    max_priority_fee_per_gas: priority_fee,
                    to: TxKind::Call(*to),
                    value: *value,
                    ..Default::default()
                };
                items.push(SubmitItem::plain(sign(funder, tx)?.0));
            }
            for chunk in items.chunks(FUNDING_CHUNK) {
                for result in client.send_raw_transactions(chunk, None).await? {
                    if let BatchSendResult::Error(e) = result {
                        return Err(BaselineError::Transaction(format!(
                            "funding transfer rejected: {e}"
                        )));
                    }
                }
            }

            let target = start_nonce + batch.len() as u64;
            let deadline = Instant::now() + FUNDING_TIMEOUT;
            loop {
                let landed = query
                    .get_transaction_count(funder.address())
                    .latest()
                    .await
                    .map_err(|e| BaselineError::Rpc(format!("funder nonce: {e}")))?;
                if landed >= target {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(BaselineError::Transaction(format!(
                        "funding round {round} incomplete after {FUNDING_TIMEOUT:?}: {} of {} transfers landed",
                        landed.saturating_sub(start_nonce),
                        batch.len()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            info!(
                funded = (round * FUNDING_ROUND + batch.len()).min(needs.len()),
                total = needs.len(),
                "funding progress"
            );
        }
        Ok(())
    }

    async fn watch_head(shared: Arc<Shared>, stop: CancellationToken) {
        let mut last_fee = Instant::now();
        while !stop.is_cancelled() {
            if let Ok(block) = shared.query.get_block_number().await {
                shared.block.fetch_max(block, Ordering::Relaxed);
            }
            if last_fee.elapsed() >= BASE_FEE_POLL_INTERVAL {
                if let Ok(fee) = shared.query.get_base_fee().await {
                    shared.base_fee.store(fee as u64, Ordering::Relaxed);
                }
                last_fee = Instant::now();
            }
            tokio::select! {
                () = tokio::time::sleep(HEAD_POLL_INTERVAL) => {}
                () = stop.cancelled() => {}
            }
        }
    }

    async fn report(shared: Arc<Shared>, started: Instant, stop: CancellationToken) {
        let stats = &shared.stats;
        let (mut last_sub, mut last_acc, mut last_block) =
            (0, 0, shared.block.load(Ordering::Relaxed));
        loop {
            tokio::select! {
                () = tokio::time::sleep(REPORT_INTERVAL) => {}
                () = stop.cancelled() => return,
            }
            let secs = REPORT_INTERVAL.as_secs_f64();
            let sub = stats.submitted.load(Ordering::Relaxed);
            let acc = stats.accepted.load(Ordering::Relaxed);
            let block = shared.block.load(Ordering::Relaxed);
            let stage = shared.stage(started.elapsed());
            let errors: Vec<_> = {
                let mut errors: Vec<_> =
                    stats.errors.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
                errors.sort_by_key(|e| std::cmp::Reverse(e.1));
                errors.into_iter().take(4).map(|(k, v)| format!("{k}={v}")).collect()
            };
            info!(
                elapsed_s = started.elapsed().as_secs(),
                active_senders = stage.map(|s| s.active_senders).unwrap_or(0),
                submitted_per_s = format!("{:.0}", (sub - last_sub) as f64 / secs),
                accepted_per_s = format!("{:.0}", (acc - last_acc) as f64 / secs),
                blocks_per_s = format!("{:.1}", block.saturating_sub(last_block) as f64 / secs),
                adds = stats.adds.load(Ordering::Relaxed),
                replacements = stats.replacements.load(Ordering::Relaxed),
                nonce_reloads = stats.nonce_reloads.load(Ordering::Relaxed),
                late_ticks = stats.late_ticks.load(Ordering::Relaxed),
                errors = errors.join(","),
                "flood progress"
            );
            (last_sub, last_acc, last_block) = (sub, acc, block);
        }
    }

    async fn worker(
        shared: Arc<Shared>,
        id: usize,
        mut shard: Vec<Sender>,
        started: Instant,
        stop: CancellationToken,
    ) {
        let client = shared.clients[id % shared.clients.len()].clone();
        let pricing = Pricing {
            priority_fee: shared.config.priority_fee,
            multiplier: shared.config.max_fee_base_fee_multiplier,
            bump_percent: u128::from(shared.config.fee_bump_percent),
            max_requotes: shared.config.max_requotes_per_window,
            lifetime_blocks: shared.config.lifetime_blocks,
        };
        // Spread worker ticks across the first interval so submissions do not burst together.
        if let Some(stage) = shared.stage(Duration::ZERO) {
            let offset = stage.interval.mul_f64(id as f64 / shared.config.workers as f64);
            tokio::select! {
                () = tokio::time::sleep(offset) => {}
                () = stop.cancelled() => return,
            }
        }

        while !stop.is_cancelled() {
            let tick = Instant::now();
            let Some(stage) = shared.stage(started.elapsed()) else { return };

            for sender in shard.iter_mut().filter(|s| s.reload_nonce) {
                if let Ok(nonce) = shared.query.get_transaction_count(sender.address).latest().await
                {
                    sender.nonce = nonce;
                    sender.window = None;
                    sender.reload_nonce = false;
                    shared.stats.nonce_reloads.fetch_add(1, Ordering::Relaxed);
                }
            }

            let block = shared.block.load(Ordering::Relaxed);
            let base_fee = u128::from(shared.base_fee.load(Ordering::Relaxed));
            let mut plans = Vec::new();
            let mut items = Vec::new();
            for (slot, sender) in shard.iter_mut().enumerate() {
                if sender.index >= stage.active_senders {
                    continue;
                }
                if sender.window.is_none() {
                    let start = *sender.not_before.get_or_insert_with(|| {
                        block + sender_phase(sender.index, pricing.lifetime_blocks)
                    });
                    if block < start {
                        continue;
                    }
                }
                let Some(kind) = sender.plan(block, base_fee, pricing) else { continue };
                match shared.build(sender) {
                    Ok(item) => {
                        plans.push(Plan { sender: slot, kind });
                        items.push(item);
                    }
                    Err(e) => shared.stats.error(format!("sign: {e}"), 1),
                }
            }

            if !items.is_empty() {
                shared.stats.submitted.fetch_add(items.len() as u64, Ordering::Relaxed);
                match client.send_raw_transactions(&items, None).await {
                    Ok(results) => {
                        for (plan, result) in plans.iter().zip(results) {
                            shared.record(&mut shard[plan.sender], &plan.kind, result);
                        }
                    }
                    Err(e) => shared.stats.error(categorize(&e.to_string()), items.len() as u64),
                }
            }

            let spent = tick.elapsed();
            if spent > stage.interval {
                shared.stats.late_ticks.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep(stage.interval - spent) => {}
                () = stop.cancelled() => return,
            }
        }
    }
}

impl Shared {
    fn stage(&self, elapsed: Duration) -> Option<ResolvedStage> {
        self.stages.iter().find(|s| elapsed < s.ends_at).copied()
    }

    fn build(&self, sender: &Sender) -> Result<SubmitItem> {
        let window = sender.window.as_ref().expect("planned sender has a window");
        let to = self.config.to.unwrap_or(sender.address);
        let block = self.block.load(Ordering::Relaxed);
        let mut validity: Vec<ValidityPredicate> = self
            .templates
            .iter()
            .map(|t| ValidityRouter::resolve_template(t, block, sender.address, Some(to)))
            .collect();
        if let Some(every) = self.config.release_every_blocks {
            validity.push(ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from((block / every + 1) * every),
            });
        }
        validity.push(ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(window.expiry),
        });
        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce: sender.nonce,
            gas_limit: self.config.gas_limit,
            max_fee_per_gas: window.max_fee,
            max_priority_fee_per_gas: window.priority_fee,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: self.calldata.clone(),
            ..Default::default()
        };
        let (raw, hash) = sign(&sender.signer, tx)?;
        let mut item = SubmitItem::with_validity(raw, validity);
        if self.config.sign_predicates {
            let digest = ValidityAuthorization::signing_hash(self.chain_id, hash, &item.validity);
            item.validity_signature = Some(
                sender
                    .signer
                    .sign_hash_sync(&digest)
                    .map_err(|e| BaselineError::Transaction(format!("sidecar signature: {e}")))?,
            );
        }
        Ok(item)
    }

    fn record(&self, sender: &mut Sender, kind: &SubmitKind, result: BatchSendResult) {
        match result {
            BatchSendResult::Success(_) => {
                self.stats.accepted.fetch_add(1, Ordering::Relaxed);
                match kind {
                    SubmitKind::Add => self.stats.adds.fetch_add(1, Ordering::Relaxed),
                    SubmitKind::Replacement => {
                        self.stats.replacements.fetch_add(1, Ordering::Relaxed)
                    }
                };
            }
            BatchSendResult::Error(e) => {
                let category = categorize(&e.message);
                if category == "nonce_too_low" {
                    sender.reload_nonce = true;
                }
                self.stats.error(category, 1);
            }
        }
    }
}

fn sign(signer: &PrivateKeySigner, tx: TxEip1559) -> Result<(Bytes, alloy_primitives::TxHash)> {
    let signature = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| BaselineError::Transaction(format!("failed to sign tx: {e}")))?;
    let envelope = TxEnvelope::Eip1559(tx.into_signed(signature));
    let hash = *envelope.tx_hash();
    Ok((Bytes::from(envelope.encoded_2718()), hash))
}

/// Retries a read with exponential backoff; shared endpoints shed load transiently.
async fn retry_read<T, E, F, Fut>(mut read: F) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, E>>,
{
    let mut delay = Duration::from_millis(200);
    let mut attempt = 1;
    loop {
        match read().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt >= RPC_READ_ATTEMPTS => return Err(e),
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
        }
    }
}

/// Maps a rejection message to a stable category for counting.
fn categorize(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let category = [
        ("nonce too low", "nonce_too_low"),
        ("underpriced", "replacement_underpriced"),
        ("already known", "already_known"),
        ("already imported", "already_known"),
        ("rate limit", "rate_limited"),
        ("429", "rate_limited"),
        ("insufficient funds", "insufficient_funds"),
        ("txpool is full", "pool_full"),
        ("pool is full", "pool_full"),
        ("exceeds", "limit_exceeded"),
        ("timed out", "timeout"),
        ("timeout", "timeout"),
    ]
    .into_iter()
    .find(|(needle, _)| lower.contains(needle))
    .map(|(_, c)| c.to_string());
    category.unwrap_or_else(|| {
        // Drop digits so messages that differ only by block number or hash share a category.
        let mut text: String = lower.chars().filter(|c| !c.is_ascii_digit()).take(80).collect();
        text.retain(|c| c != '=' && c != ',');
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRICING: Pricing = Pricing {
        priority_fee: 100,
        multiplier: 2,
        bump_percent: 12,
        max_requotes: 2,
        lifetime_blocks: 10,
    };

    fn sender() -> Sender {
        let signer = PrivateKeySigner::random();
        Sender {
            index: 0,
            address: signer.address(),
            signer,
            nonce: 0,
            window: None,
            reload_nonce: false,
            not_before: None,
        }
    }

    #[test]
    fn windows_add_then_bump_then_wait_for_expiry() {
        let mut s = sender();
        assert!(matches!(s.plan(100, 1_000, PRICING), Some(SubmitKind::Add)));
        assert_eq!(s.window.as_ref().unwrap().expiry, 110);
        assert_eq!(s.window.as_ref().unwrap().max_fee, 2_100);

        assert!(matches!(s.plan(101, 1_000, PRICING), Some(SubmitKind::Replacement)));
        assert_eq!(s.window.as_ref().unwrap().max_fee, 2_352);
        assert_eq!(s.window.as_ref().unwrap().priority_fee, 112);
        assert!(matches!(s.plan(102, 1_000, PRICING), Some(SubmitKind::Replacement)));
        assert!(s.plan(103, 1_000, PRICING).is_none());

        assert!(matches!(s.plan(110, 1_000, PRICING), Some(SubmitKind::Add)));
        assert_eq!(s.window.as_ref().unwrap().max_fee, 2_100);
    }

    #[test]
    fn categorizes_common_rejections() {
        assert_eq!(categorize("nonce too low: next nonce 5"), "nonce_too_low");
        assert_eq!(categorize("replacement transaction underpriced"), "replacement_underpriced");
        assert_eq!(categorize("over rate limit"), "rate_limited");
    }

    #[test]
    fn rejects_predicates_over_production_cap() {
        let predicate = "    - type: balance\n      op: '>='\n      value: '0'\n";
        let yaml = format!(
            "submission_rpcs: ['http://localhost:1']\nquery_rpc: 'http://localhost:1'\n\
             sender_count: 2\nstages: [{{duration: 1s, active_senders: 2, requote_interval: 100ms}}]\n\
             predicates:\n{}",
            predicate.repeat(16)
        );
        let config: ValidityFloodConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_err());
        let mut allowed = config;
        allowed.allow_over_production_cap = true;
        allowed.validate().unwrap();
    }
}
