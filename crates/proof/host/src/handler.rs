use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718, eip4844::FIELD_ELEMENTS_PER_BLOB};
use alloy_network::Network;
use alloy_primitives::{Address, B64, B256, Bytes, keccak256};
use alloy_provider::Provider;
use alloy_rlp::Decodable;
use alloy_rpc_types::{Block, debug::ExecutionWitness};
use ark_ff::{BigInteger, PrimeField};
use base_common_consensus::{HoloceneExtraData, JovianExtraData, Predeploys};
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_providers::BlobWithCommitmentAndProof;
use base_proof::{Hint, HintType, ROOTS_OF_UNITY};
use base_proof_preimage::{PreimageKey, PreimageKeyType};
use base_protocol::{BlockInfo, OutputRoot};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::{
    HostConfig, HostError, HostProviders, Metrics, Result, SharedKeyValueStore, store_ordered_trie,
};

const HOST_SERVER_TARGET: &str = "host_server";
const PAYLOAD_WITNESS_PREFETCH_LOOKAHEAD_BLOCKS: u64 = 10;
const PAYLOAD_WITNESS_PREFETCH_MAX_IN_FLIGHT: usize = 10;
const PAYLOAD_WITNESS_PREFETCH_MAX_READY: usize = 16;
const PAYLOAD_WITNESS_PREFETCH_MAX_COMPLETED_BLOCKS: usize = 1024;
const PAYLOAD_WITNESS_PREFETCH_MAX_LOOKAHEAD_PARENTS: usize = 64;
const EXECUTION_WITNESS_PREIMAGE_WRITE_BATCH_SIZE: usize = 256;

#[derive(Debug, Clone, Copy)]
struct ExecutionWitnessStats {
    state_count: usize,
    code_count: usize,
    key_count: usize,
    state_bytes: usize,
    code_bytes: usize,
    key_bytes: usize,
}

impl ExecutionWitnessStats {
    const fn total_preimage_count(&self) -> usize {
        self.state_count + self.code_count + self.key_count
    }

    const fn total_preimage_bytes(&self) -> usize {
        self.state_bytes + self.code_bytes + self.key_bytes
    }
}

#[derive(Debug, Clone)]
struct PayloadWitnessReady {
    block_number: u64,
    parent_block_hash: B256,
    payload_timestamp: u64,
    tx_count: usize,
    stats: ExecutionWitnessStats,
    rpc_elapsed: Duration,
    insert_elapsed: Duration,
}

#[derive(Debug, Clone)]
enum PayloadWitnessCacheEntry {
    InFlight { keys: Arc<[B256]> },
    Ready { ready: PayloadWitnessReady, keys: Arc<[B256]> },
}

impl PayloadWitnessCacheEntry {
    fn keys(&self) -> Arc<[B256]> {
        match self {
            Self::InFlight { keys } | Self::Ready { keys, .. } => Arc::clone(keys),
        }
    }
}

#[derive(Debug, Default)]
struct PayloadWitnessPrefetchState {
    entries: HashMap<B256, PayloadWitnessCacheEntry>,
    ready_order: VecDeque<B256>,
    scheduled_blocks: HashSet<u64>,
    completed_blocks: HashSet<u64>,
    completed_block_order: VecDeque<u64>,
    scheduled_lookahead_parents: HashSet<B256>,
    scheduled_lookahead_parent_order: VecDeque<B256>,
}

#[derive(Debug)]
struct PayloadWitnessPrefetchInner {
    state: Mutex<PayloadWitnessPrefetchState>,
    semaphore: Semaphore,
}

/// Best-effort host-only prefetch cache for `debug_executePayload` witnesses.
///
/// The guest still sends and validates the real `L2PayloadWitness` hint. Prefetch results are only
/// reused when their normalized payload witness key matches the hint that the guest later emits.
#[derive(Debug, Clone)]
pub(crate) struct PayloadWitnessPrefetcher {
    inner: Arc<PayloadWitnessPrefetchInner>,
}

#[derive(Debug)]
struct PayloadWitnessInFlightGuard {
    prefetcher: PayloadWitnessPrefetcher,
    keys: Arc<[B256]>,
    completed: bool,
}

impl PayloadWitnessInFlightGuard {
    const fn new(prefetcher: PayloadWitnessPrefetcher, keys: Arc<[B256]>) -> Self {
        Self { prefetcher, keys, completed: false }
    }

    fn mark_ready(mut self, ready: PayloadWitnessReady) {
        self.completed = true;
        self.prefetcher.mark_ready(&self.keys, ready);
    }

    fn mark_failed(mut self) {
        self.completed = true;
        self.prefetcher.mark_failed(&self.keys);
    }
}

impl Drop for PayloadWitnessInFlightGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        self.prefetcher.mark_failed(&self.keys);
    }
}

impl Default for PayloadWitnessPrefetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PayloadWitnessPrefetcher {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(PayloadWitnessPrefetchInner {
                state: Mutex::new(PayloadWitnessPrefetchState::default()),
                semaphore: Semaphore::new(PAYLOAD_WITNESS_PREFETCH_MAX_IN_FLIGHT),
            }),
        }
    }

    fn take_ready(&self, keys: &[B256]) -> Option<PayloadWitnessReady> {
        let mut state = self.lock_state();
        match keys.iter().find_map(|key| state.entries.get(key).cloned()) {
            Some(PayloadWitnessCacheEntry::Ready { ready, keys }) => {
                Self::remove_cache_keys(&mut state, &keys);
                Some(ready)
            }
            Some(PayloadWitnessCacheEntry::InFlight { .. }) | None => None,
        }
    }

    pub(crate) async fn schedule_lookahead(
        &self,
        cfg: &HostConfig,
        providers: &HostProviders,
        kv: SharedKeyValueStore,
        parent_block_hash: B256,
    ) {
        if !cfg.prover.enable_experimental_witness_endpoint {
            return;
        }

        if !self.mark_lookahead_parent_scheduled(parent_block_hash) {
            return;
        }

        let cfg = Arc::new(cfg.clone());
        let providers = Arc::new(providers.clone());
        let prefetcher = self.clone();
        tokio::spawn(async move {
            let parent_block = match providers.l2.get_block_by_hash(parent_block_hash).await {
                Ok(Some(block)) => block,
                Ok(None) => {
                    prefetcher.unmark_lookahead_parent_scheduled(parent_block_hash);
                    debug!(
                        target: HOST_SERVER_TARGET,
                        ?parent_block_hash,
                        "payload witness prefetch skipped: parent block not found"
                    );
                    return;
                }
                Err(err) => {
                    prefetcher.unmark_lookahead_parent_scheduled(parent_block_hash);
                    debug!(
                        target: HOST_SERVER_TARGET,
                        ?parent_block_hash,
                        error = %err,
                        "payload witness prefetch skipped: failed to fetch parent block"
                    );
                    return;
                }
            };

            // The witness hint identifies the current payload by parent hash plus payload
            // attributes, so the current payload number is the parent block number plus one.
            let current_block_number = parent_block.header.inner.number + 1;
            let first_prefetch_block = current_block_number + 1;
            let last_prefetch_block =
                current_block_number + PAYLOAD_WITNESS_PREFETCH_LOOKAHEAD_BLOCKS;

            for block_number in first_prefetch_block..=last_prefetch_block {
                if !prefetcher.mark_block_scheduled(block_number) {
                    continue;
                }

                prefetcher.spawn_prefetch_block(
                    Arc::clone(&cfg),
                    Arc::clone(&providers),
                    Arc::clone(&kv),
                    block_number,
                );
            }
        });
    }

    fn mark_lookahead_parent_scheduled(&self, parent_block_hash: B256) -> bool {
        let mut state = self.lock_state();
        if !state.scheduled_lookahead_parents.insert(parent_block_hash) {
            return false;
        }

        state.scheduled_lookahead_parent_order.push_back(parent_block_hash);
        while state.scheduled_lookahead_parent_order.len()
            > PAYLOAD_WITNESS_PREFETCH_MAX_LOOKAHEAD_PARENTS
        {
            if let Some(evicted_parent) = state.scheduled_lookahead_parent_order.pop_front() {
                state.scheduled_lookahead_parents.remove(&evicted_parent);
            }
        }

        true
    }

    fn unmark_lookahead_parent_scheduled(&self, parent_block_hash: B256) {
        let mut state = self.lock_state();
        state.scheduled_lookahead_parents.remove(&parent_block_hash);
        state.scheduled_lookahead_parent_order.retain(|parent| *parent != parent_block_hash);
    }

    fn spawn_prefetch_block(
        &self,
        cfg: Arc<HostConfig>,
        providers: Arc<HostProviders>,
        kv: SharedKeyValueStore,
        block_number: u64,
    ) {
        let prefetcher = self.clone();
        let cleanup_prefetcher = self.clone();
        let handle = tokio::spawn(async move {
            prefetcher.prefetch_block(cfg, providers, kv, block_number).await;
        });
        tokio::spawn(async move {
            if let Err(err) = handle.await {
                cleanup_prefetcher.unmark_block_scheduled(block_number);
                warn!(
                    target: HOST_SERVER_TARGET,
                    block_number = %block_number,
                    error = %err,
                    "payload witness prefetch task failed"
                );
            }
        });
    }

    fn mark_block_scheduled(&self, block_number: u64) -> bool {
        let mut state = self.lock_state();
        if state.completed_blocks.contains(&block_number) {
            return false;
        }

        state.scheduled_blocks.insert(block_number)
    }

    fn unmark_block_scheduled(&self, block_number: u64) {
        let mut state = self.lock_state();
        state.scheduled_blocks.remove(&block_number);
    }

    fn mark_block_completed(&self, block_number: u64) {
        let mut state = self.lock_state();
        state.scheduled_blocks.remove(&block_number);
        if state.completed_blocks.insert(block_number) {
            state.completed_block_order.push_back(block_number);
        }

        while state.completed_block_order.len() > PAYLOAD_WITNESS_PREFETCH_MAX_COMPLETED_BLOCKS {
            if let Some(evicted_block) = state.completed_block_order.pop_front() {
                state.completed_blocks.remove(&evicted_block);
            }
        }
    }

    fn try_mark_in_flight(&self, cache_keys: Arc<[B256]>) -> Option<PayloadWitnessInFlightGuard> {
        let mut state = self.lock_state();
        if cache_keys.iter().any(|key| state.entries.contains_key(key)) {
            return None;
        }

        for key in cache_keys.iter() {
            state
                .entries
                .insert(*key, PayloadWitnessCacheEntry::InFlight { keys: Arc::clone(&cache_keys) });
        }
        Some(PayloadWitnessInFlightGuard::new(self.clone(), cache_keys))
    }

    fn mark_ready(&self, keys: &[B256], ready: PayloadWitnessReady) {
        let mut state = self.lock_state();
        let cache_keys = Arc::<[B256]>::from(keys);
        let existing_key_sets = cache_keys
            .iter()
            .filter_map(|key| state.entries.get(key).map(PayloadWitnessCacheEntry::keys))
            .collect::<Vec<_>>();
        for existing_keys in existing_key_sets {
            Self::remove_cache_keys(&mut state, &existing_keys);
        }

        for key in cache_keys.iter() {
            state.entries.insert(
                *key,
                PayloadWitnessCacheEntry::Ready {
                    ready: ready.clone(),
                    keys: Arc::clone(&cache_keys),
                },
            );
        }
        if let Some(key) = cache_keys.first() {
            state.ready_order.push_back(*key);
        }

        while state.ready_order.len() > PAYLOAD_WITNESS_PREFETCH_MAX_READY {
            let evicted_keys = state
                .ready_order
                .pop_front()
                .and_then(|key| state.entries.get(&key).map(PayloadWitnessCacheEntry::keys));
            if let Some(keys) = evicted_keys {
                Self::remove_cache_keys(&mut state, &keys);
            }
        }
    }

    fn mark_failed(&self, keys: &[B256]) {
        let mut state = self.lock_state();
        let failed_keys = keys.iter().find_map(|key| match state.entries.get(key) {
            Some(PayloadWitnessCacheEntry::InFlight { keys }) => Some(Arc::clone(keys)),
            _ => None,
        });
        if let Some(failed_keys) = failed_keys {
            Self::remove_cache_keys(&mut state, &failed_keys);
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, PayloadWitnessPrefetchState> {
        self.inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn remove_cache_keys(state: &mut PayloadWitnessPrefetchState, keys: &[B256]) {
        for key in keys {
            state.entries.remove(key);
        }
        state.ready_order.retain(|ready_key| !keys.contains(ready_key));
    }

    async fn prefetch_block(
        &self,
        cfg: Arc<HostConfig>,
        providers: Arc<HostProviders>,
        kv: SharedKeyValueStore,
        block_number: u64,
    ) {
        if self.prefetch_block_inner(cfg, providers, kv, block_number).await {
            self.mark_block_completed(block_number);
        } else {
            self.unmark_block_scheduled(block_number);
        }
    }

    async fn prefetch_block_inner(
        &self,
        cfg: Arc<HostConfig>,
        providers: Arc<HostProviders>,
        kv: SharedKeyValueStore,
        block_number: u64,
    ) -> bool {
        let _permit = match self.inner.semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => return false,
        };

        let block = match providers
            .l2
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .full()
            .await
        {
            Ok(Some(block)) => block,
            Ok(None) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    "payload witness prefetch skipped: block not found"
                );
                return false;
            }
            Err(err) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    error = %err,
                    "payload witness prefetch skipped: failed to fetch block"
                );
                return false;
            }
        };

        let parent_block_hash = block.header.inner.parent_hash;
        let payload_attributes = match payload_attributes_from_l2_block(&cfg, block) {
            Ok(payload_attributes) => payload_attributes,
            Err(err) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    error = %err,
                    "payload witness prefetch skipped: failed to reconstruct payload attributes"
                );
                return false;
            }
        };
        let payload_timestamp = payload_attributes.payload_attributes.timestamp;
        let tx_count =
            payload_attributes.transactions.as_ref().map_or(0, |transactions| transactions.len());
        let keys = match payload_witness_keys(parent_block_hash, &payload_attributes, &cfg) {
            Ok(keys) => keys,
            Err(err) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    error = %err,
                    "payload witness prefetch skipped: failed to compute cache key"
                );
                return false;
            }
        };

        let Some(in_flight) = self.try_mark_in_flight(Arc::<[B256]>::from(keys)) else {
            return false;
        };

        info!(
            target: HOST_SERVER_TARGET,
            block_number,
            ?parent_block_hash,
            payload_timestamp,
            tx_count,
            "payload witness prefetch started"
        );

        let rpc_start = Instant::now();
        let execute_payload_response = match providers
            .l2
            .client()
            .request::<(B256, BasePayloadAttributes), ExecutionWitness>(
                "debug_executePayload",
                (parent_block_hash, payload_attributes),
            )
            .await
        {
            Ok(response) => response,
            Err(err) => {
                warn!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    ?parent_block_hash,
                    payload_timestamp,
                    tx_count,
                    error = %err,
                    "payload witness prefetch failed: debug_executePayload failed"
                );
                in_flight.mark_failed();
                return false;
            }
        };
        let rpc_elapsed = rpc_start.elapsed();

        let stats = execution_witness_stats(&execute_payload_response);
        let insert_start = Instant::now();
        if let Err(err) =
            insert_execution_witness_preimages(Arc::clone(&kv), execute_payload_response).await
        {
            warn!(
                target: HOST_SERVER_TARGET,
                block_number,
                ?parent_block_hash,
                payload_timestamp,
                tx_count,
                error = %err,
                "payload witness prefetch failed: preimage insertion failed"
            );
            in_flight.mark_failed();
            return false;
        }
        let insert_elapsed = insert_start.elapsed();

        in_flight.mark_ready(PayloadWitnessReady {
            block_number,
            parent_block_hash,
            payload_timestamp,
            tx_count,
            stats,
            rpc_elapsed,
            insert_elapsed,
        });

        info!(
            target: HOST_SERVER_TARGET,
            block_number,
            ?parent_block_hash,
            payload_timestamp,
            tx_count,
            state_count = stats.state_count,
            code_count = stats.code_count,
            key_count = stats.key_count,
            state_bytes = stats.state_bytes,
            code_bytes = stats.code_bytes,
            key_bytes = stats.key_bytes,
            total_preimage_count = stats.total_preimage_count(),
            total_preimage_bytes = stats.total_preimage_bytes(),
            rpc_elapsed_ms = rpc_elapsed.as_millis(),
            insert_elapsed_ms = insert_elapsed.as_millis(),
            total_elapsed_ms = (rpc_elapsed + insert_elapsed).as_millis(),
            "payload witness prefetch completed"
        );

        true
    }
}

fn payload_witness_key(
    parent_block_hash: B256,
    payload_attributes: &BasePayloadAttributes,
) -> Result<B256> {
    let encoded_attributes = serde_json::to_vec(payload_attributes)?;
    let mut key_data = Vec::with_capacity(32 + encoded_attributes.len());
    key_data.extend_from_slice(parent_block_hash.as_slice());
    key_data.extend_from_slice(&encoded_attributes);
    Ok(keccak256(key_data))
}

fn payload_witness_keys(
    parent_block_hash: B256,
    payload_attributes: &BasePayloadAttributes,
    cfg: &HostConfig,
) -> Result<Vec<B256>> {
    let mut keys = vec![payload_witness_key(parent_block_hash, payload_attributes)?];
    let default_base_fee_params = cfg.prover.rollup_config.chain_op_config.post_canyon_params();
    let (Ok(default_elasticity), Ok(default_denominator)) = (
        u32::try_from(default_base_fee_params.elasticity_multiplier),
        u32::try_from(default_base_fee_params.max_change_denominator),
    ) else {
        return Ok(keys);
    };

    let default_params = encode_payload_eip_1559_params(default_elasticity, default_denominator);
    match payload_attributes.eip_1559_params {
        Some(params) if params == default_params => {
            let mut zero_params_attributes = payload_attributes.clone();
            zero_params_attributes.eip_1559_params = Some(B64::ZERO);
            push_payload_witness_key(&mut keys, parent_block_hash, &zero_params_attributes)?;
        }
        Some(params) if params == B64::ZERO => {
            let mut default_params_attributes = payload_attributes.clone();
            default_params_attributes.eip_1559_params = Some(default_params);
            push_payload_witness_key(&mut keys, parent_block_hash, &default_params_attributes)?;
        }
        _ => {}
    }

    Ok(keys)
}

fn push_payload_witness_key(
    keys: &mut Vec<B256>,
    parent_block_hash: B256,
    payload_attributes: &BasePayloadAttributes,
) -> Result<()> {
    let key = payload_witness_key(parent_block_hash, payload_attributes)?;
    if !keys.contains(&key) {
        keys.push(key);
    }
    Ok(())
}

fn execution_witness_stats(execute_payload_response: &ExecutionWitness) -> ExecutionWitnessStats {
    ExecutionWitnessStats {
        state_count: execute_payload_response.state.len(),
        code_count: execute_payload_response.codes.len(),
        key_count: execute_payload_response.keys.len(),
        state_bytes: execute_payload_response.state.iter().map(|preimage| preimage.len()).sum(),
        code_bytes: execute_payload_response.codes.iter().map(|preimage| preimage.len()).sum(),
        key_bytes: execute_payload_response.keys.iter().map(|preimage| preimage.len()).sum(),
    }
}

async fn insert_execution_witness_preimages(
    kv: SharedKeyValueStore,
    execute_payload_response: ExecutionWitness,
) -> Result<()> {
    let preimages = execute_payload_response
        .state
        .into_iter()
        .chain(execute_payload_response.codes)
        .chain(execute_payload_response.keys);

    let mut batch = Vec::with_capacity(EXECUTION_WITNESS_PREIMAGE_WRITE_BATCH_SIZE);
    for preimage in preimages {
        let preimage_bytes: Vec<u8> = preimage.into();
        let computed_hash = keccak256(&preimage_bytes);

        let key = PreimageKey::new_keccak256(*computed_hash);
        batch.push((key.into(), preimage_bytes));
        if batch.len() == EXECUTION_WITNESS_PREIMAGE_WRITE_BATCH_SIZE {
            write_execution_witness_preimage_batch(&kv, batch).await?;
            batch = Vec::with_capacity(EXECUTION_WITNESS_PREIMAGE_WRITE_BATCH_SIZE);
            tokio::task::yield_now().await;
        }
    }

    if !batch.is_empty() {
        write_execution_witness_preimage_batch(&kv, batch).await?;
    }

    Ok(())
}

async fn write_execution_witness_preimage_batch(
    kv: &SharedKeyValueStore,
    preimages: Vec<(B256, Vec<u8>)>,
) -> Result<()> {
    let mut kv_lock = kv.write().await;
    for (key, preimage) in preimages {
        kv_lock.set(key, preimage)?;
    }
    Ok(())
}

fn payload_attributes_from_l2_block(
    cfg: &HostConfig,
    block: Block<<Base as Network>::TransactionResponse, <Base as Network>::HeaderResponse>,
) -> Result<BasePayloadAttributes> {
    let timestamp = block.header.inner.timestamp;
    let mut payload_attributes = BasePayloadAttributes::default();
    payload_attributes.payload_attributes.timestamp = timestamp;
    payload_attributes.payload_attributes.prev_randao = block.header.inner.mix_hash;
    payload_attributes.payload_attributes.suggested_fee_recipient = block.header.inner.beneficiary;
    payload_attributes.payload_attributes.parent_beacon_block_root =
        block.header.inner.parent_beacon_block_root;
    payload_attributes.payload_attributes.withdrawals =
        block.withdrawals.as_ref().map(|withdrawals| withdrawals.0.clone());
    payload_attributes.transactions = Some(
        block
            .transactions
            .into_transactions()
            .map(|tx| tx.as_ref().encoded_2718().into())
            .collect(),
    );
    payload_attributes.no_tx_pool = Some(true);
    payload_attributes.gas_limit = Some(block.header.inner.gas_limit);

    if cfg.prover.rollup_config.is_jovian_active(timestamp) {
        let (elasticity, denominator, min_base_fee) =
            JovianExtraData::decode(&block.header.inner.extra_data)
                .map_err(|err| HostError::Custom(err.to_string()))?;
        payload_attributes.eip_1559_params =
            Some(encode_payload_eip_1559_params(elasticity, denominator));
        payload_attributes.min_base_fee = Some(min_base_fee);
    } else if cfg.prover.rollup_config.is_holocene_active(timestamp) {
        let (elasticity, denominator) = HoloceneExtraData::decode(&block.header.inner.extra_data)
            .map_err(|err| HostError::Custom(err.to_string()))?;
        payload_attributes.eip_1559_params =
            Some(encode_payload_eip_1559_params(elasticity, denominator));
    }

    Ok(payload_attributes)
}

fn encode_payload_eip_1559_params(elasticity: u32, denominator: u32) -> B64 {
    let mut encoded = [0u8; 8];
    encoded[..4].copy_from_slice(&denominator.to_be_bytes());
    encoded[4..].copy_from_slice(&elasticity.to_be_bytes());
    B64::from(encoded)
}

/// Parses a blob hint, supporting both legacy (48-byte) and new (40-byte) formats.
///
/// Returns the blob hash and timestamp.
///
/// ## Formats
/// - Legacy: hash (32 bytes) + index (8 bytes) + timestamp (8 bytes) = 48 bytes
/// - New: hash (32 bytes) + timestamp (8 bytes) = 40 bytes
///
/// The legacy index field is parsed but ignored.
pub fn parse_blob_hint(hint_data: &[u8]) -> Result<(B256, u64)> {
    match hint_data.len() {
        48 => {
            let hash_data_bytes: [u8; 32] = hint_data[0..32].try_into()?;
            let _index_data_bytes: [u8; 8] = hint_data[32..40].try_into()?;
            let timestamp_data_bytes: [u8; 8] = hint_data[40..48].try_into()?;

            let hash: B256 = hash_data_bytes.into();
            let timestamp = u64::from_be_bytes(timestamp_data_bytes);
            Ok((hash, timestamp))
        }
        40 => {
            let hash_data_bytes: [u8; 32] = hint_data[0..32].try_into()?;
            let timestamp_data_bytes: [u8; 8] = hint_data[32..40].try_into()?;

            let hash: B256 = hash_data_bytes.into();
            let timestamp = u64::from_be_bytes(timestamp_data_bytes);
            Ok((hash, timestamp))
        }
        _ => Err(HostError::Custom(format!(
            "Invalid blob hint length: expected 40 or 48 bytes, got {}",
            hint_data.len()
        ))),
    }
}

/// Fetches data in response to a hint.
pub async fn handle_hint(
    hint: Hint<HintType>,
    cfg: &HostConfig,
    providers: &HostProviders,
    kv: SharedKeyValueStore,
) -> Result<()> {
    handle_hint_with_prefetcher(hint, cfg, providers, kv, None).await
}

pub(crate) async fn handle_hint_with_prefetcher(
    hint: Hint<HintType>,
    cfg: &HostConfig,
    providers: &HostProviders,
    kv: SharedKeyValueStore,
    payload_witness_prefetcher: Option<PayloadWitnessPrefetcher>,
) -> Result<()> {
    let hint_type_label: &str = hint.ty.into();

    Metrics::hint_requests_total(hint_type_label).increment(1);
    let _timer = base_metrics::timed!(Metrics::hint_duration_seconds(hint_type_label));

    let result =
        Box::pin(handle_hint_inner(hint, cfg, providers, kv, payload_witness_prefetcher)).await;

    if result.is_err() {
        Metrics::hint_errors_total(hint_type_label).increment(1);
    }

    result
}

async fn handle_hint_inner(
    hint: Hint<HintType>,
    cfg: &HostConfig,
    providers: &HostProviders,
    kv: SharedKeyValueStore,
    payload_witness_prefetcher: Option<PayloadWitnessPrefetcher>,
) -> Result<()> {
    match hint.ty {
        HintType::L1BlockHeader => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let raw_header: Bytes =
                providers.l1.client().request("debug_getRawHeader", [hash]).await?;

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
        }
        HintType::L1Transactions => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let Block { transactions, .. } = providers
                .l1
                .get_block_by_hash(hash)
                .full()
                .await?
                .ok_or(HostError::BlockNotFound)?;
            let encoded_transactions = transactions
                .into_transactions()
                .map(|tx| tx.inner.encoded_2718())
                .collect::<Vec<_>>();

            store_ordered_trie(kv.as_ref(), encoded_transactions.as_slice()).await?;
        }
        HintType::L1Receipts => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let raw_receipts: Vec<Bytes> =
                providers.l1.client().request("debug_getRawReceipts", [hash]).await?;

            store_ordered_trie(kv.as_ref(), raw_receipts.as_slice()).await?;
        }
        HintType::L1Blob => {
            let (hash, timestamp) = parse_blob_hint(&hint.data)?;

            let partial_block_ref = BlockInfo { timestamp, ..Default::default() };

            let mut blobs = providers
                .blobs
                .fetch_blobs_with_proofs(&partial_block_ref, &[hash])
                .await
                .map_err(|e| HostError::BlobSidecarFetchFailed(e.to_string()))?;
            if blobs.len() != 1 {
                return Err(HostError::BlobCountMismatch { expected: 1, actual: blobs.len() });
            }
            let BlobWithCommitmentAndProof { blob, kzg_proof: proof, kzg_commitment: commitment } =
                blobs.pop().expect("Expected 1 blob");

            let mut kv_lock = kv.write().await;

            kv_lock.set(
                PreimageKey::new(*hash, PreimageKeyType::Sha256).into(),
                commitment.to_vec(),
            )?;

            let mut blob_key = [0u8; 80];
            blob_key[..48].copy_from_slice(commitment.as_ref());
            for i in 0..FIELD_ELEMENTS_PER_BLOB {
                blob_key[48..].copy_from_slice(
                    ROOTS_OF_UNITY[i as usize].into_bigint().to_bytes_be().as_ref(),
                );
                let blob_key_hash = keccak256(blob_key.as_ref());

                kv_lock.set(PreimageKey::new_keccak256(*blob_key_hash).into(), blob_key.into())?;
                kv_lock.set(
                    PreimageKey::new(*blob_key_hash, PreimageKeyType::Blob).into(),
                    blob.as_ref()[(i as usize) << 5..(i as usize + 1) << 5].to_vec(),
                )?;
            }

            blob_key[72..].copy_from_slice(FIELD_ELEMENTS_PER_BLOB.to_be_bytes().as_ref());
            let blob_key_hash = keccak256(blob_key.as_ref());

            kv_lock.set(PreimageKey::new_keccak256(*blob_key_hash).into(), blob_key.into())?;
            kv_lock.set(
                PreimageKey::new(*blob_key_hash, PreimageKeyType::Blob).into(),
                proof.to_vec(),
            )?;
        }
        HintType::L1Precompile => {
            if hint.data.len() < 28 {
                return Err(HostError::InvalidHintDataLength);
            }

            let input_hash = keccak256(hint.data.as_ref());

            #[cfg(feature = "precompiles")]
            let result = {
                let address = Address::from_slice(&hint.data.as_ref()[..20]);
                let gas = u64::from_be_bytes(hint.data.as_ref()[20..28].try_into()?);
                let input = hint.data[28..].to_vec();
                crate::precompiles::execute(address, input, gas).map_or_else(
                    |_| vec![0u8; 1],
                    |raw_res: Vec<u8>| {
                        let mut res = Vec::with_capacity(1 + raw_res.len());
                        res.push(0x01);
                        res.extend_from_slice(&raw_res);
                        res
                    },
                )
            };
            #[cfg(not(feature = "precompiles"))]
            let result = vec![0u8; 1];

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*input_hash).into(), hint.data.into())?;
            kv_lock
                .set(PreimageKey::new(*input_hash, PreimageKeyType::Precompile).into(), result)?;
        }
        HintType::L2BlockHeader => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let raw_header: Bytes =
                providers.l2.client().request("debug_getRawHeader", [hash]).await?;

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
        }
        HintType::L2Transactions => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let Block { transactions, .. } = providers
                .l2
                .get_block_by_hash(hash)
                .full()
                .await?
                .ok_or(HostError::BlockNotFound)?;

            let encoded_transactions = transactions
                .into_transactions()
                .map(|tx| tx.inner.inner.encoded_2718())
                .collect::<Vec<_>>();
            store_ordered_trie(kv.as_ref(), encoded_transactions.as_slice()).await?;
        }
        HintType::StartingL2Output => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let raw_header: Bytes = providers
                .l2
                .client()
                .request("debug_getRawHeader", &[cfg.request.agreed_l2_head_hash])
                .await?;
            let header = Header::decode(&mut raw_header.as_ref())?;

            let l2_to_l1_message_passer = providers
                .l2
                .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Default::default())
                .block_id(cfg.request.agreed_l2_head_hash.into())
                .await?;

            let output_root = OutputRoot::from_parts(
                header.state_root,
                l2_to_l1_message_passer.storage_hash,
                cfg.request.agreed_l2_head_hash,
            );
            let output_root_hash = output_root.hash();

            if output_root_hash != cfg.request.agreed_l2_output_root {
                return Err(HostError::OutputRootMismatch);
            }

            let mut kv_write_lock = kv.write().await;
            kv_write_lock.set(
                PreimageKey::new_keccak256(*output_root_hash).into(),
                output_root.encode().into(),
            )?;
        }
        HintType::L2Code => {
            const CODE_PREFIX: u8 = b'c';

            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;

            let code_key = [&[CODE_PREFIX], hash.as_slice()].concat();
            let code = providers
                .l2
                .client()
                .request::<&[Bytes; 1], Bytes>("debug_dbGet", &[code_key.into()])
                .await;

            let code = match code {
                Ok(code) => code,
                Err(_) => providers
                    .l2
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_dbGet", &[hash])
                    .await
                    .map_err(|e| HostError::CodeHashPreimageFetchFailed(e.to_string()))?,
            };

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*hash).into(), code.into())?;
        }
        HintType::L2StateNode => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;

            warn!(node_hash = %hash, "L2StateNode hint sent");
            warn!("debug_executePayload failed to return a complete witness");

            let preimage: Bytes = providers.l2.client().request("debug_dbGet", &[hash]).await?;
            let actual_hash = keccak256(preimage.as_ref());
            if actual_hash != hash {
                return Err(HostError::StateNodePreimageHashMismatch {
                    expected: hash,
                    actual: actual_hash,
                });
            }

            let mut kv_write_lock = kv.write().await;
            kv_write_lock.set(PreimageKey::new_keccak256(*hash).into(), preimage.into())?;
        }
        HintType::L2AccountProof => {
            if hint.data.len() != 8 + 20 {
                return Err(HostError::InvalidHintDataLength);
            }

            let block_number = u64::from_be_bytes(hint.data.as_ref()[..8].try_into()?);
            let address = Address::from_slice(&hint.data.as_ref()[8..28]);

            let proof_response = providers
                .l2
                .get_proof(address, Default::default())
                .block_id(block_number.into())
                .await?;

            let mut kv_lock = kv.write().await;
            proof_response.account_proof.into_iter().try_for_each(|node| {
                let node_hash = keccak256(node.as_ref());
                let key = PreimageKey::new_keccak256(*node_hash);
                kv_lock.set(key.into(), node.into())?;
                Ok::<(), HostError>(())
            })?;
        }
        HintType::L2AccountStorageProof => {
            if hint.data.len() != 8 + 20 + 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let block_number = u64::from_be_bytes(hint.data.as_ref()[..8].try_into()?);
            let address = Address::from_slice(&hint.data.as_ref()[8..28]);
            let slot = B256::from_slice(&hint.data.as_ref()[28..]);

            let proof_response =
                providers.l2.get_proof(address, vec![slot]).block_id(block_number.into()).await?;

            let mut kv_lock = kv.write().await;

            proof_response.account_proof.into_iter().try_for_each(|node| {
                let node_hash = keccak256(node.as_ref());
                let key = PreimageKey::new_keccak256(*node_hash);
                kv_lock.set(key.into(), node.into())?;
                Ok::<(), HostError>(())
            })?;

            let storage_proof = proof_response
                .storage_proof
                .into_iter()
                .next()
                .ok_or_else(|| HostError::Custom("empty storage proof from RPC".into()))?;
            storage_proof.proof.into_iter().try_for_each(|node| {
                let node_hash = keccak256(node.as_ref());
                let key = PreimageKey::new_keccak256(*node_hash);
                kv_lock.set(key.into(), node.into())?;
                Ok::<(), HostError>(())
            })?;
        }
        HintType::L2PayloadWitness => {
            if !cfg.prover.enable_experimental_witness_endpoint {
                warn!("L2PayloadWitness hint sent but payload witness is disabled, skipping");
                return Ok(());
            }

            if hint.data.len() < 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let parent_block_hash = B256::from_slice(&hint.data.as_ref()[..32]);
            let payload_attributes: BasePayloadAttributes =
                serde_json::from_slice(&hint.data[32..])?;
            let payload_witness_cache_keys =
                payload_witness_keys(parent_block_hash, &payload_attributes, cfg)?;

            let tx_count = payload_attributes
                .transactions
                .as_ref()
                .map_or(0, |transactions| transactions.len());
            let payload_timestamp = payload_attributes.payload_attributes.timestamp;

            if let Some(prefetcher) = payload_witness_prefetcher.as_ref()
                && let Some(ready) = prefetcher.take_ready(&payload_witness_cache_keys)
            {
                info!(
                    target: HOST_SERVER_TARGET,
                    block_number = ready.block_number,
                    parent_block_hash = ?ready.parent_block_hash,
                    payload_timestamp = ready.payload_timestamp,
                    tx_count = ready.tx_count,
                    state_count = ready.stats.state_count,
                    code_count = ready.stats.code_count,
                    key_count = ready.stats.key_count,
                    state_bytes = ready.stats.state_bytes,
                    code_bytes = ready.stats.code_bytes,
                    key_bytes = ready.stats.key_bytes,
                    total_preimage_count = ready.stats.total_preimage_count(),
                    total_preimage_bytes = ready.stats.total_preimage_bytes(),
                    prefetch_rpc_elapsed_ms = ready.rpc_elapsed.as_millis(),
                    prefetch_insert_elapsed_ms = ready.insert_elapsed.as_millis(),
                    "debug_executePayload witness served from host prefetch cache"
                );
                prefetcher
                    .schedule_lookahead(cfg, providers, Arc::clone(&kv), parent_block_hash)
                    .await;
                return Ok(());
            }

            let rpc_start = Instant::now();
            let execute_payload_response = match providers
                .l2
                .client()
                .request::<(B256, BasePayloadAttributes), ExecutionWitness>(
                    "debug_executePayload",
                    (parent_block_hash, payload_attributes),
                )
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    warn!(
                        target: HOST_SERVER_TARGET,
                        parent_block_hash = ?parent_block_hash,
                        payload_timestamp,
                        tx_count,
                        error = %err,
                        "debug_executePayload failed"
                    );
                    return Err(HostError::Custom(format!("debug_executePayload failed: {err}")));
                }
            };
            let rpc_elapsed = rpc_start.elapsed();

            let stats = execution_witness_stats(&execute_payload_response);

            let insert_start = Instant::now();
            insert_execution_witness_preimages(Arc::clone(&kv), execute_payload_response).await?;
            let insert_elapsed = insert_start.elapsed();

            info!(
                target: HOST_SERVER_TARGET,
                parent_block_hash = ?parent_block_hash,
                payload_timestamp,
                tx_count,
                state_count = stats.state_count,
                code_count = stats.code_count,
                key_count = stats.key_count,
                state_bytes = stats.state_bytes,
                code_bytes = stats.code_bytes,
                key_bytes = stats.key_bytes,
                total_preimage_count = stats.total_preimage_count(),
                total_preimage_bytes = stats.total_preimage_bytes(),
                rpc_elapsed_ms = rpc_elapsed.as_millis(),
                insert_elapsed_ms = insert_elapsed.as_millis(),
                total_elapsed_ms = (rpc_elapsed + insert_elapsed).as_millis(),
                "debug_executePayload witness captured"
            );

            if let Some(prefetcher) = payload_witness_prefetcher {
                prefetcher
                    .schedule_lookahead(cfg, providers, Arc::clone(&kv), parent_block_hash)
                    .await;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use alloy_genesis::ChainConfig;
    use alloy_provider::{RootProvider, builder as provider_builder, mock::Asserter};
    use base_common_genesis::RollupConfig;
    use base_common_network::Base;
    use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
    use base_proof_primitives::ProofRequest;
    use tokio::sync::RwLock;

    use super::*;
    use crate::{MemoryKeyValueStore, ProverConfig};

    const TEST_HASH: B256 = B256::new([0x42u8; 32]);
    const TEST_TIMESTAMP: u64 = 1234567890;

    const LEGACY_HINT: [u8; 48] = [
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFA, 0xCA, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x96, 0x02, 0xD2,
    ];

    const NEW_HINT: [u8; 40] = [
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x00, 0x00, 0x00, 0x00, 0x49, 0x96, 0x02, 0xD2,
    ];

    fn test_cfg() -> HostConfig {
        HostConfig {
            request: ProofRequest::default(),
            prover: ProverConfig {
                l1_eth_url: "http://127.0.0.1:1".to_string(),
                l2_eth_url: "http://127.0.0.1:1".to_string(),
                l1_beacon_url: "http://127.0.0.1:1".to_string(),
                l2_chain_id: 0,
                rollup_config: RollupConfig::default(),
                l1_config: ChainConfig::default(),
                enable_experimental_witness_endpoint: false,
            },
            data_dir: None,
        }
    }

    fn test_providers(l2: RootProvider<Base>) -> HostProviders {
        let l1 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let beacon = OnlineBeaconClient::new_http("http://127.0.0.1:1".to_string());
        let blobs =
            OnlineBlobProvider { beacon_client: beacon, genesis_time: 0, slot_interval: 12 };
        HostProviders { l1, blobs, l2 }
    }

    fn test_payload_witness_ready(block_number: u64) -> PayloadWitnessReady {
        PayloadWitnessReady {
            block_number,
            parent_block_hash: TEST_HASH,
            payload_timestamp: TEST_TIMESTAMP,
            tx_count: 1,
            stats: ExecutionWitnessStats {
                state_count: 1,
                code_count: 2,
                key_count: 3,
                state_bytes: 4,
                code_bytes: 5,
                key_bytes: 6,
            },
            rpc_elapsed: Duration::ZERO,
            insert_elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn test_parse_blob_hint_formats() {
        let (legacy_hash, legacy_timestamp) = parse_blob_hint(&LEGACY_HINT).unwrap();
        let (new_hash, new_timestamp) = parse_blob_hint(&NEW_HINT).unwrap();

        assert_eq!(legacy_hash, TEST_HASH);
        assert_eq!(legacy_timestamp, TEST_TIMESTAMP);
        assert_eq!(new_hash, TEST_HASH);
        assert_eq!(new_timestamp, TEST_TIMESTAMP);
    }

    #[test]
    fn test_parse_blob_hint_invalid_length() {
        let hint_data = vec![0u8; 35];
        let result = parse_blob_hint(&hint_data);

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid blob hint length"));
        assert!(err_msg.contains("expected 40 or 48 bytes"));
        assert!(err_msg.contains("got 35"));
    }

    #[test]
    fn test_payload_witness_keys_include_default_and_zero_param_aliases() {
        let cfg = test_cfg();
        let default_base_fee_params = cfg.prover.rollup_config.chain_op_config.post_canyon_params();
        let default_elasticity =
            u32::try_from(default_base_fee_params.elasticity_multiplier).unwrap();
        let default_denominator =
            u32::try_from(default_base_fee_params.max_change_denominator).unwrap();
        let default_params =
            encode_payload_eip_1559_params(default_elasticity, default_denominator);
        let parent_block_hash = B256::new([0x11u8; 32]);
        let default_params_attributes =
            BasePayloadAttributes { eip_1559_params: Some(default_params), ..Default::default() };
        let mut zero_params_attributes = default_params_attributes.clone();
        zero_params_attributes.eip_1559_params = Some(B64::ZERO);

        let default_keys =
            payload_witness_keys(parent_block_hash, &default_params_attributes, &cfg).unwrap();
        let zero_keys =
            payload_witness_keys(parent_block_hash, &zero_params_attributes, &cfg).unwrap();
        let default_key_set = default_keys.into_iter().collect::<HashSet<_>>();
        let zero_key_set = zero_keys.into_iter().collect::<HashSet<_>>();

        assert_eq!(default_key_set.len(), 2);
        assert_eq!(default_key_set, zero_key_set);
    }

    #[test]
    fn test_payload_witness_prefetch_alias_lookup_removes_aliases() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        let keys = [B256::new([0x01u8; 32]), B256::new([0x02u8; 32])];
        prefetcher.mark_ready(&keys, test_payload_witness_ready(1));

        let ready = prefetcher.take_ready(&[keys[1]]).unwrap();
        let state = prefetcher.lock_state();

        assert_eq!(ready.block_number, 1);
        assert!(!state.entries.contains_key(&keys[0]));
        assert!(!state.entries.contains_key(&keys[1]));
        assert!(state.ready_order.is_empty());
    }

    #[test]
    fn test_payload_witness_prefetch_in_flight_lookup_returns_none() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        let keys = [B256::new([0x03u8; 32]), B256::new([0x04u8; 32])];
        let in_flight =
            prefetcher.try_mark_in_flight(Arc::<[B256]>::from(keys.as_slice())).unwrap();

        let ready = prefetcher.take_ready(&[keys[0]]);

        assert!(ready.is_none());
        {
            let state = prefetcher.lock_state();
            assert!(state.entries.contains_key(&keys[0]));
            assert!(state.entries.contains_key(&keys[1]));
        }

        in_flight.mark_ready(test_payload_witness_ready(2));
        let ready = prefetcher.take_ready(&[keys[1]]).unwrap();

        assert_eq!(ready.block_number, 2);
    }

    #[tokio::test]
    async fn test_payload_witness_lookahead_parent_deduplicates_and_evicts() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        let parent = B256::new([0xAAu8; 32]);

        assert!(prefetcher.mark_lookahead_parent_scheduled(parent));
        assert!(!prefetcher.mark_lookahead_parent_scheduled(parent));

        for index in 0..PAYLOAD_WITNESS_PREFETCH_MAX_LOOKAHEAD_PARENTS {
            let next_parent = B256::new([index as u8; 32]);
            assert!(prefetcher.mark_lookahead_parent_scheduled(next_parent));
        }

        assert!(prefetcher.mark_lookahead_parent_scheduled(parent));
    }

    #[test]
    fn test_payload_witness_completed_blocks_deduplicate_and_evict() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        let block_number = u64::MAX;

        assert!(prefetcher.mark_block_scheduled(block_number));
        prefetcher.mark_block_completed(block_number);
        assert!(!prefetcher.mark_block_scheduled(block_number));

        for index in 0..PAYLOAD_WITNESS_PREFETCH_MAX_COMPLETED_BLOCKS {
            let next_block = u64::try_from(index).unwrap();
            assert!(prefetcher.mark_block_scheduled(next_block));
            prefetcher.mark_block_completed(next_block);
        }

        assert!(prefetcher.mark_block_scheduled(block_number));
    }

    #[test]
    fn test_payload_witness_in_flight_guard_drop_removes_aliases() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        let keys = [B256::new([0x05u8; 32]), B256::new([0x06u8; 32])];
        let guard = prefetcher.try_mark_in_flight(Arc::<[B256]>::from(keys.as_slice())).unwrap();

        drop(guard);

        let state = prefetcher.lock_state();
        assert!(!state.entries.contains_key(&keys[0]));
        assert!(!state.entries.contains_key(&keys[1]));
    }

    #[test]
    fn test_payload_witness_ready_eviction_removes_aliases() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        for index in 0..=PAYLOAD_WITNESS_PREFETCH_MAX_READY {
            let first_key = B256::new([index as u8; 32]);
            let second_key =
                B256::new([(index + PAYLOAD_WITNESS_PREFETCH_MAX_READY + 1) as u8; 32]);
            prefetcher
                .mark_ready(&[first_key, second_key], test_payload_witness_ready(index as u64));
        }

        let evicted_keys =
            [B256::new([0u8; 32]), B256::new([(PAYLOAD_WITNESS_PREFETCH_MAX_READY + 1) as u8; 32])];
        let state = prefetcher.lock_state();

        assert_eq!(state.ready_order.len(), PAYLOAD_WITNESS_PREFETCH_MAX_READY);
        assert!(!state.entries.contains_key(&evicted_keys[0]));
        assert!(!state.entries.contains_key(&evicted_keys[1]));
    }

    #[test]
    fn test_payload_witness_ready_replaces_overlapping_aliases() {
        let prefetcher = PayloadWitnessPrefetcher::new();
        let shared_key = B256::new([0x11u8; 32]);
        let old_alias = B256::new([0x12u8; 32]);
        let new_alias = B256::new([0x13u8; 32]);

        prefetcher.mark_ready(&[shared_key, old_alias], test_payload_witness_ready(1));
        prefetcher.mark_ready(&[shared_key, new_alias], test_payload_witness_ready(2));

        assert!(prefetcher.take_ready(&[old_alias]).is_none());
        let ready = prefetcher.take_ready(&[new_alias]).unwrap();
        let state = prefetcher.lock_state();

        assert_eq!(ready.block_number, 2);
        assert!(state.entries.is_empty());
        assert!(state.ready_order.is_empty());
    }

    #[tokio::test]
    async fn test_l2_payload_witness_propagates_rpc_error() {
        let mut cfg = test_cfg();
        cfg.prover.enable_experimental_witness_endpoint = true;
        let payload_attributes = BasePayloadAttributes::default();
        let payload_attributes_json = serde_json::to_vec(&payload_attributes).unwrap();
        let mut hint_data = TEST_HASH.as_slice().to_vec();
        hint_data.extend_from_slice(&payload_attributes_json);
        let hint = HintType::L2PayloadWitness.with_data(&[hint_data.as_slice()]);
        let asserter = Asserter::new();
        asserter.push_failure_msg("injected debug_executePayload failure");
        let l2 = provider_builder::<Base>().connect_mocked_client(asserter);
        let providers = test_providers(l2);
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));

        let err = handle_hint(hint, &cfg, &providers, Arc::clone(&kv)).await.unwrap_err();

        assert!(err.to_string().contains("debug_executePayload failed"));
        assert!(err.to_string().contains("injected debug_executePayload failure"));
    }

    #[tokio::test]
    async fn test_l2_state_node_rejects_hash_mismatch() {
        const MALFORMED_PREIMAGE: [u8; 3] = [0xC2, 0x80, 0x80];

        let requested_hash = TEST_HASH;
        let preimage = Bytes::from(MALFORMED_PREIMAGE.to_vec());
        let actual_hash = keccak256(preimage.as_ref());
        let asserter = Asserter::new();
        asserter.push_success(&preimage);
        let l2 = provider_builder::<Base>().connect_mocked_client(asserter);
        let providers = test_providers(l2);
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));
        let hint = HintType::L2StateNode.with_data(&[requested_hash.as_slice()]);

        let err = handle_hint(hint, &test_cfg(), &providers, Arc::clone(&kv)).await.unwrap_err();

        match err {
            HostError::StateNodePreimageHashMismatch { expected, actual } => {
                assert_eq!(expected, requested_hash);
                assert_eq!(actual, actual_hash);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(kv.read().await.get(PreimageKey::new_keccak256(*requested_hash).into()).is_none());
    }
}
