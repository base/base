use std::{
    any::Any,
    collections::{HashMap, HashSet, VecDeque},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use alloy_consensus::Header;
use alloy_eips::{
    BlockId, BlockNumberOrTag, eip2718::Encodable2718, eip4844::FIELD_ELEMENTS_PER_BLOB,
};
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
use futures::FutureExt;
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::{debug, info, warn};

use crate::{
    HostConfig, HostError, HostProviders, Metrics, Result, SharedKeyValueStore, store_ordered_trie,
};

const HOST_SERVER_TARGET: &str = "host_server";
const PAYLOAD_WITNESS_PREFETCH_MAX_IN_FLIGHT: usize = 10;
const PAYLOAD_WITNESS_PREFETCH_MAX_READY: usize = 1024;
const PAYLOAD_WITNESS_PREFETCH_PREIMAGE_WRITE_BATCH_SIZE: usize = 1024;
const _: () = {
    assert!(PAYLOAD_WITNESS_PREFETCH_MAX_READY >= 600);
};
const L1_HEADER_PREFETCH_MAX_IN_FLIGHT: usize = 32;

#[derive(Debug, Default)]
struct PayloadWitnessPrefetchState {
    ready: HashMap<B256, B256>,
    ready_order: VecDeque<B256>,
}

#[derive(Debug)]
struct PayloadWitnessPrefetchInner {
    cfg: Arc<HostConfig>,
    providers: Arc<HostProviders>,
    state: Mutex<PayloadWitnessPrefetchState>,
    semaphore: Semaphore,
}

/// Best-effort host-only prefetch cache for `debug_executePayload` witnesses.
///
/// The guest still sends and validates the real `L2PayloadWitness` hint. Prefetch results are only
/// used to skip the foreground RPC when the prefetched payload attributes match the hinted payload
/// attributes for the same parent block hash.
#[derive(Debug, Clone)]
pub(crate) struct PayloadWitnessPrefetcher {
    inner: Arc<PayloadWitnessPrefetchInner>,
}

impl PayloadWitnessPrefetcher {
    pub(crate) fn new(cfg: Arc<HostConfig>, providers: Arc<HostProviders>) -> Self {
        Self {
            inner: Arc::new(PayloadWitnessPrefetchInner {
                cfg,
                providers,
                state: Mutex::new(PayloadWitnessPrefetchState::default()),
                semaphore: Semaphore::new(PAYLOAD_WITNESS_PREFETCH_MAX_IN_FLIGHT),
            }),
        }
    }

    fn take_ready(&self, parent_block_hash: B256, payload_attributes_digest: B256) -> bool {
        let mut state = self.lock_state();
        let Some(ready_payload_attributes_digest) = state.ready.get(&parent_block_hash) else {
            return false;
        };
        if *ready_payload_attributes_digest != payload_attributes_digest {
            return false;
        }
        state.ready.remove(&parent_block_hash);
        state.ready_order.retain(|hash| hash != &parent_block_hash);
        true
    }

    pub(crate) async fn prefetch_request_range(&self, kv: SharedKeyValueStore) {
        if !self.inner.cfg.prover.enable_experimental_witness_endpoint {
            return;
        }

        let Some((first_block, last_block)) = self.request_range().await else {
            return;
        };

        info!(
            target: HOST_SERVER_TARGET,
            first_block,
            last_block,
            "payload witness range prefetch started"
        );

        let mut tasks = JoinSet::new();
        for block_number in first_block..=last_block {
            let prefetcher = self.clone();
            let kv = Arc::clone(&kv);
            tasks.spawn(async move {
                let result = AssertUnwindSafe(prefetcher.prefetch_block_inner(kv, block_number))
                    .catch_unwind()
                    .await;

                if let Err(panic) = result {
                    warn!(
                        target: HOST_SERVER_TARGET,
                        block_number,
                        panic = %panic_payload_message(panic.as_ref()),
                        "payload witness range prefetch task panicked"
                    );
                }
            });
        }

        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                warn!(
                    target: HOST_SERVER_TARGET,
                    error = %error,
                    "payload witness range prefetch task join failed"
                );
            }
        }

        info!(
            target: HOST_SERVER_TARGET,
            first_block,
            last_block,
            "payload witness range prefetch finished"
        );
    }

    async fn request_range(&self) -> Option<(u64, u64)> {
        let last_block = self.inner.cfg.request.claimed_l2_block_number;
        let parent_hash = self.inner.cfg.request.agreed_l2_head_hash;
        let parent_block = match self.inner.providers.l2.get_block_by_hash(parent_hash).await {
            Ok(Some(block)) => block,
            Ok(None) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    ?parent_hash,
                    "payload witness range prefetch skipped: agreed L2 head not found"
                );
                return None;
            }
            Err(err) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    ?parent_hash,
                    error = %err,
                    "payload witness range prefetch skipped: failed to fetch agreed L2 head"
                );
                return None;
            }
        };

        let Some(first_block) = next_block_number(parent_block.header.inner.number) else {
            debug!(
                target: HOST_SERVER_TARGET,
                parent_block_number = parent_block.header.inner.number,
                "payload witness range prefetch skipped: block number overflow"
            );
            return None;
        };

        if first_block > last_block {
            return None;
        }

        Some((first_block, Self::request_prefetch_last_block(first_block, last_block)))
    }

    fn request_prefetch_last_block(first_block: u64, last_block: u64) -> u64 {
        let max_blocks = PAYLOAD_WITNESS_PREFETCH_MAX_READY as u64;
        first_block.saturating_add(max_blocks.saturating_sub(1)).min(last_block)
    }

    fn mark_ready(&self, parent_block_hash: B256, payload_attributes_digest: B256) {
        let mut state = self.lock_state();
        if state.ready.remove(&parent_block_hash).is_some() {
            state.ready_order.retain(|hash| hash != &parent_block_hash);
        }

        while state.ready.len() >= PAYLOAD_WITNESS_PREFETCH_MAX_READY {
            let Some(oldest) = state.ready_order.pop_front() else {
                break;
            };
            state.ready.remove(&oldest);
        }

        state.ready.insert(parent_block_hash, payload_attributes_digest);
        state.ready_order.push_back(parent_block_hash);
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PayloadWitnessPrefetchState> {
        // The prefetch cache is best-effort, and every mutation under this lock is an individual
        // collection insert or remove. Recovering from poisoning can leave a stale cache entry, but
        // it cannot leave structurally invalid state that affects proof correctness.
        self.inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn prefetch_block_inner(&self, kv: SharedKeyValueStore, block_number: u64) -> bool {
        // Range tasks wait here, so the semaphore bounds concurrent prefetch RPC work.
        let _permit = match self.inner.semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => return false,
        };

        let block = match self
            .inner
            .providers
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
        let payload_attributes = match payload_attributes_from_l2_block(&self.inner.cfg, block) {
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
        // This is a best-effort duplicate-RPC guard; another task may still mark the same parent
        // ready before this task finishes.
        if self.lock_state().ready.contains_key(&parent_block_hash) {
            return false;
        }

        let payload_attributes_digest = match payload_attributes_digest(&payload_attributes) {
            Ok(digest) => digest,
            Err(err) => {
                warn!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    ?parent_block_hash,
                    error = %err,
                    "payload witness prefetch skipped: failed to digest payload attributes"
                );
                return false;
            }
        };

        let execute_payload_response = match self
            .inner
            .providers
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
                    error = %err,
                    "payload witness prefetch failed: debug_executePayload failed"
                );
                return false;
            }
        };

        if let Err(err) =
            insert_execution_witness_preimages_batched(Arc::clone(&kv), execute_payload_response)
                .await
        {
            warn!(
                target: HOST_SERVER_TARGET,
                block_number,
                ?parent_block_hash,
                error = %err,
                "payload witness prefetch failed: preimage insertion failed"
            );
            return false;
        }

        self.mark_ready(parent_block_hash, payload_attributes_digest);

        true
    }
}

#[derive(Debug)]
struct L1HeaderPrefetchInner {
    providers: Arc<HostProviders>,
    scheduled_blocks: Mutex<HashSet<u64>>,
    semaphore: Semaphore,
}

/// Best-effort host-only L1 header range prefetcher.
///
/// The guest still requests headers by hash and validates the parent chain. Range prefetch fetches
/// block numbers only after confirming the range end is the pinned L1 head, then stores each header
/// under its actual hash so a reorged block cannot satisfy a different guest request.
#[derive(Debug, Clone)]
pub(crate) struct L1HeaderPrefetcher {
    inner: Arc<L1HeaderPrefetchInner>,
}

impl L1HeaderPrefetcher {
    pub(crate) fn new(providers: Arc<HostProviders>) -> Self {
        Self {
            inner: Arc::new(L1HeaderPrefetchInner {
                providers,
                scheduled_blocks: Mutex::new(HashSet::new()),
                semaphore: Semaphore::new(L1_HEADER_PREFETCH_MAX_IN_FLIGHT),
            }),
        }
    }

    pub(crate) async fn prefetch_range(
        &self,
        kv: SharedKeyValueStore,
        start_number: u64,
        end_number: u64,
        end_hash: B256,
    ) {
        if start_number >= end_number {
            return;
        }
        if !self.canonical_head_matches(end_number, end_hash).await {
            return;
        }

        let mut tasks = JoinSet::new();
        for block_number in start_number..end_number {
            if self.mark_block_scheduled(block_number) {
                let prefetcher = self.clone();
                let kv = Arc::clone(&kv);
                tasks.spawn(async move {
                    let result =
                        AssertUnwindSafe(prefetcher.prefetch_block_inner(kv, block_number))
                            .catch_unwind()
                            .await;
                    prefetcher.unmark_block_scheduled(block_number);

                    if let Err(panic) = result {
                        warn!(
                            target: HOST_SERVER_TARGET,
                            block_number,
                            panic = %panic_payload_message(panic.as_ref()),
                            "l1 header prefetch task panicked"
                        );
                    }
                });

                if tasks.len() >= L1_HEADER_PREFETCH_MAX_IN_FLIGHT {
                    Self::join_next_prefetch_task(&mut tasks).await;
                }
            }
        }

        while !tasks.is_empty() {
            Self::join_next_prefetch_task(&mut tasks).await;
        }
    }

    async fn join_next_prefetch_task(tasks: &mut JoinSet<()>) {
        if let Some(result) = tasks.join_next().await {
            if let Err(err) = result {
                warn!(
                    target: HOST_SERVER_TARGET,
                    error = %err,
                    "l1 header prefetch task join failed"
                );
            }
        }
    }

    async fn canonical_head_matches(&self, end_number: u64, end_hash: B256) -> bool {
        match self.fetch_raw_header_by_number(end_number).await {
            Ok((raw_header, header)) if header.hash_slow() == end_hash => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    end_number,
                    end_hash = %end_hash,
                    raw_header_len = raw_header.len(),
                    "l1 header range prefetch canonical head confirmed"
                );
                true
            }
            Ok((_raw_header, header)) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    end_number,
                    expected = %end_hash,
                    actual = %header.hash_slow(),
                    "l1 header range prefetch skipped: canonical head mismatch"
                );
                false
            }
            Err(err) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    end_number,
                    error = %err,
                    "l1 header range prefetch skipped: failed to fetch canonical head"
                );
                false
            }
        }
    }

    async fn prefetch_block_inner(&self, kv: SharedKeyValueStore, block_number: u64) -> bool {
        let (raw_header, decoded_header) = match self.fetch_raw_header_by_number(block_number).await
        {
            Ok(header) => header,
            Err(err) => {
                debug!(
                    target: HOST_SERVER_TARGET,
                    block_number,
                    error = %err,
                    "l1 header prefetch skipped: failed to fetch raw header"
                );
                return false;
            }
        };

        let hash = decoded_header.hash_slow();

        if let Err(err) = insert_l1_header_preimage(Arc::clone(&kv), hash, raw_header).await {
            warn!(
                target: HOST_SERVER_TARGET,
                block_number,
                hash = %hash,
                error = %err,
                "l1 header prefetch failed: preimage insertion failed"
            );
            return false;
        }

        true
    }

    async fn fetch_raw_header_by_number(&self, block_number: u64) -> Result<(Bytes, Header)> {
        let _permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .map_err(|err| HostError::Custom(err.to_string()))?;

        let raw_header: Bytes = self
            .inner
            .providers
            .l1
            .client()
            .request("debug_getRawHeader", (BlockId::number(block_number),))
            .await?;
        let decoded_header = match Header::decode(&mut raw_header.as_ref()) {
            Ok(header) => header,
            Err(err) => {
                return Err(HostError::Rlp(err));
            }
        };
        if decoded_header.number != block_number {
            return Err(HostError::Custom(format!(
                "raw header number mismatch: requested {block_number}, decoded {}",
                decoded_header.number
            )));
        }

        Ok((raw_header, decoded_header))
    }

    fn mark_block_scheduled(&self, block_number: u64) -> bool {
        self.lock_scheduled_blocks().insert(block_number)
    }

    fn unmark_block_scheduled(&self, block_number: u64) {
        self.lock_scheduled_blocks().remove(&block_number);
    }

    fn lock_scheduled_blocks(&self) -> std::sync::MutexGuard<'_, HashSet<u64>> {
        self.inner.scheduled_blocks.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn execution_witness_preimages(
    execute_payload_response: ExecutionWitness,
) -> impl Iterator<Item = (PreimageKey, Vec<u8>)> {
    execute_payload_response
        .state
        .into_iter()
        .chain(execute_payload_response.codes)
        .chain(execute_payload_response.keys)
        .map(|preimage| {
            let preimage_bytes: Vec<u8> = preimage.into();
            let computed_hash = keccak256(&preimage_bytes);
            (PreimageKey::new_keccak256(*computed_hash), preimage_bytes)
        })
}

async fn insert_execution_witness_preimages(
    kv: SharedKeyValueStore,
    execute_payload_response: ExecutionWitness,
) -> Result<()> {
    let mut kv_lock = kv.write().await;
    for (key, preimage_bytes) in execution_witness_preimages(execute_payload_response) {
        kv_lock.set(key.into(), preimage_bytes)?;
    }
    Ok(())
}

async fn insert_execution_witness_preimages_batched(
    kv: SharedKeyValueStore,
    execute_payload_response: ExecutionWitness,
) -> Result<()> {
    let mut preimages = execution_witness_preimages(execute_payload_response).peekable();

    while preimages.peek().is_some() {
        {
            let mut kv_lock = kv.write().await;
            for _ in 0..PAYLOAD_WITNESS_PREFETCH_PREIMAGE_WRITE_BATCH_SIZE {
                let Some((key, preimage_bytes)) = preimages.next() else {
                    break;
                };
                kv_lock.set(key.into(), preimage_bytes)?;
            }
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "unknown panic payload"
    }
}

fn payload_attributes_digest(payload_attributes: &BasePayloadAttributes) -> Result<B256> {
    Ok(keccak256(serde_json::to_vec(payload_attributes)?))
}

const fn next_block_number(block_number: u64) -> Option<u64> {
    block_number.checked_add(1)
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

async fn insert_l1_header_preimage(
    kv: SharedKeyValueStore,
    hash: B256,
    raw_header: Bytes,
) -> Result<()> {
    let mut kv_lock = kv.write().await;
    kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
    Ok(())
}

fn parse_l1_header_range_hint(hint_data: &[u8]) -> Result<(u64, u64, B256)> {
    if hint_data.len() != 48 {
        return Err(HostError::InvalidHintDataLength);
    }

    let start_number = u64::from_be_bytes(hint_data[..8].try_into()?);
    let end_number = u64::from_be_bytes(hint_data[8..16].try_into()?);
    let end_hash = B256::from_slice(&hint_data[16..48]);

    Ok((start_number, end_number, end_hash))
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
    handle_hint_with_prefetchers(hint, cfg, providers, kv, None, None).await
}

pub(crate) async fn handle_hint_with_prefetchers(
    hint: Hint<HintType>,
    cfg: &HostConfig,
    providers: &HostProviders,
    kv: SharedKeyValueStore,
    payload_witness_prefetcher: Option<PayloadWitnessPrefetcher>,
    l1_header_prefetcher: Option<L1HeaderPrefetcher>,
) -> Result<()> {
    let hint_type_label: &str = hint.ty.into();

    Metrics::hint_requests_total(hint_type_label).increment(1);
    let _timer = base_metrics::timed!(Metrics::hint_duration_seconds(hint_type_label));

    let result = Box::pin(handle_hint_inner(
        hint,
        cfg,
        providers,
        kv,
        payload_witness_prefetcher,
        l1_header_prefetcher,
    ))
    .await;

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
    l1_header_prefetcher: Option<L1HeaderPrefetcher>,
) -> Result<()> {
    match hint.ty {
        HintType::L1BlockHeader => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;

            if kv.read().await.get(PreimageKey::new_keccak256(*hash).into()).is_some() {
                debug!(
                    target: HOST_SERVER_TARGET,
                    hash = %hash,
                    "l1 header served from preimage kv"
                );
                return Ok(());
            }

            let raw_header: Bytes =
                providers.l1.client().request("debug_getRawHeader", [hash]).await?;
            let header = Header::decode(&mut raw_header.as_ref())?;
            let decoded_hash = header.hash_slow();
            if decoded_hash != hash {
                return Err(HostError::HeaderPreimageHashMismatch {
                    expected: hash,
                    actual: decoded_hash,
                });
            }

            insert_l1_header_preimage(Arc::clone(&kv), hash, raw_header).await?;
        }
        HintType::L1BlockHeaderRange => {
            let (start_number, end_number, end_hash) = parse_l1_header_range_hint(&hint.data)?;
            if let Some(prefetcher) = l1_header_prefetcher {
                prefetcher
                    .prefetch_range(Arc::clone(&kv), start_number, end_number, end_hash)
                    .await;
            }
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
            let encoded_payload_attributes = &hint.data[32..];

            if let Some(prefetcher) = payload_witness_prefetcher.as_ref()
                && prefetcher.take_ready(parent_block_hash, keccak256(encoded_payload_attributes))
            {
                // Prefetched preimages are written into the same proof-session KV store, which is
                // append-only for the lifetime of a proof request. The guest emits this hint with
                // serde_json::to_vec(BasePayloadAttributes), matching the digest stored by
                // prefetch, so the ready cache does not retain or compare full transaction bytes.
                debug!(
                    target: HOST_SERVER_TARGET,
                    ?parent_block_hash,
                    "payload witness served from prefetch cache"
                );
                return Ok(());
            }

            let payload_attributes: BasePayloadAttributes =
                serde_json::from_slice(encoded_payload_attributes)?;

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
                Err(e) => {
                    warn!(error = %e, "debug_executePayload failed");
                    return Ok(());
                }
            };

            insert_execution_witness_preimages(Arc::clone(&kv), execute_payload_response).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_genesis::ChainConfig;
    use alloy_provider::{RootProvider, builder as provider_builder, mock::Asserter};
    use alloy_rlp::Encodable;
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

    fn test_providers_with_l1(l1: RootProvider, l2: RootProvider<Base>) -> HostProviders {
        let beacon = OnlineBeaconClient::new_http("http://127.0.0.1:1".to_string());
        let blobs =
            OnlineBlobProvider { beacon_client: beacon, genesis_time: 0, slot_interval: 12 };
        HostProviders { l1, blobs, l2 }
    }

    fn test_providers(l2: RootProvider<Base>) -> HostProviders {
        let l1 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        test_providers_with_l1(l1, l2)
    }

    fn test_prefetcher() -> PayloadWitnessPrefetcher {
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        PayloadWitnessPrefetcher::new(Arc::new(test_cfg()), Arc::new(test_providers(l2)))
    }

    fn test_l1_header_prefetcher() -> L1HeaderPrefetcher {
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        L1HeaderPrefetcher::new(Arc::new(test_providers(l2)))
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
    fn test_parse_l1_header_range_hint() {
        let mut hint_data = Vec::new();
        hint_data.extend_from_slice(&7u64.to_be_bytes());
        hint_data.extend_from_slice(&11u64.to_be_bytes());
        hint_data.extend_from_slice(TEST_HASH.as_slice());

        let (start, end, hash) = parse_l1_header_range_hint(&hint_data).unwrap();

        assert_eq!(start, 7);
        assert_eq!(end, 11);
        assert_eq!(hash, TEST_HASH);
    }

    #[test]
    fn test_next_block_number_handles_successor_and_overflow() {
        assert_eq!(next_block_number(41), Some(42));
        assert_eq!(next_block_number(u64::MAX), None);
    }

    #[test]
    fn test_payload_witness_request_range_prefetch_caps_to_cache_window() {
        let last_block = PayloadWitnessPrefetcher::request_prefetch_last_block(42, u64::MAX);

        assert_eq!(last_block, 41 + PAYLOAD_WITNESS_PREFETCH_MAX_READY as u64);
    }

    #[test]
    fn test_payload_witness_ready_cache_evicts_oldest_entry() {
        let prefetcher = test_prefetcher();
        let payload_attributes = BasePayloadAttributes::default();
        let digest = payload_attributes_digest(&payload_attributes).unwrap();

        for i in 0..=PAYLOAD_WITNESS_PREFETCH_MAX_READY as u64 {
            let mut hash = [0; 32];
            hash[24..].copy_from_slice(&i.to_be_bytes());
            prefetcher.mark_ready(B256::new(hash), digest);
        }

        assert!(!prefetcher.take_ready(B256::ZERO, digest));

        let mut second_hash = [0; 32];
        second_hash[24..].copy_from_slice(&1u64.to_be_bytes());
        assert!(prefetcher.take_ready(B256::new(second_hash), digest));
    }

    #[test]
    fn test_payload_witness_ready_cache_keeps_mismatched_attributes() {
        let prefetcher = test_prefetcher();
        let parent_block_hash = B256::new([1; 32]);
        let payload_attributes = BasePayloadAttributes::default();
        let digest = payload_attributes_digest(&payload_attributes).unwrap();
        let mismatched_payload_attributes =
            BasePayloadAttributes { gas_limit: Some(1), ..Default::default() };
        let mismatched_digest = payload_attributes_digest(&mismatched_payload_attributes).unwrap();

        prefetcher.mark_ready(parent_block_hash, digest);

        assert!(!prefetcher.take_ready(parent_block_hash, mismatched_digest));
        assert!(prefetcher.take_ready(parent_block_hash, digest));
    }

    #[test]
    fn test_l1_header_prefetcher_dedupes_block_number() {
        let prefetcher = test_l1_header_prefetcher();

        assert!(prefetcher.mark_block_scheduled(7));
        assert!(!prefetcher.mark_block_scheduled(7));

        prefetcher.unmark_block_scheduled(7);

        assert!(prefetcher.mark_block_scheduled(7));
    }

    #[tokio::test]
    async fn test_insert_l1_header_preimage_uses_header_hash() {
        let header = Header { number: 7, parent_hash: TEST_HASH, ..Default::default() };
        let hash = header.hash_slow();
        let mut encoded_header = Vec::new();
        header.encode(&mut encoded_header);
        let raw_header = Bytes::from(encoded_header);
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));

        insert_l1_header_preimage(Arc::clone(&kv), hash, raw_header.clone()).await.unwrap();

        let stored = kv
            .read()
            .await
            .get(PreimageKey::new_keccak256(*hash).into())
            .expect("header preimage should be stored under its hash");
        assert_eq!(stored, raw_header);
    }

    #[tokio::test]
    async fn test_l1_header_hint_uses_existing_kv_preimage() {
        let header = Header { number: 7, parent_hash: TEST_HASH, ..Default::default() };
        let hash = header.hash_slow();
        let mut encoded_header = Vec::new();
        header.encode(&mut encoded_header);
        let raw_header = Bytes::from(encoded_header);
        let l1 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let providers = test_providers_with_l1(l1, l2);
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));
        kv.write()
            .await
            .set(PreimageKey::new_keccak256(*hash).into(), raw_header.clone().into())
            .unwrap();
        let hint = HintType::L1BlockHeader.with_data(&[hash.as_slice()]);

        handle_hint(hint, &test_cfg(), &providers, Arc::clone(&kv)).await.unwrap();

        let stored = kv.read().await.get(PreimageKey::new_keccak256(*hash).into()).unwrap();
        assert_eq!(stored, raw_header);
    }

    #[tokio::test]
    async fn test_l1_header_range_prefetches_missing_numbers() {
        let head = Header { number: 11, parent_hash: TEST_HASH, ..Default::default() };
        let head_hash = head.hash_slow();
        let mut encoded_head = Vec::new();
        head.encode(&mut encoded_head);

        let block = Header { number: 10, parent_hash: B256::new([0x24; 32]), ..Default::default() };
        let block_hash = block.hash_slow();
        let mut encoded_block = Vec::new();
        block.encode(&mut encoded_block);
        let raw_block = Bytes::from(encoded_block);

        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(encoded_head));
        asserter.push_success(&raw_block);
        let l1 = provider_builder().connect_mocked_client(asserter);
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let providers = test_providers_with_l1(l1, l2);
        let prefetcher = L1HeaderPrefetcher::new(Arc::new(providers));
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));

        prefetcher.prefetch_range(Arc::clone(&kv), 10, 11, head_hash).await;

        let stored = kv.read().await.get(PreimageKey::new_keccak256(*block_hash).into()).unwrap();
        assert_eq!(stored, raw_block);
        assert!(prefetcher.lock_scheduled_blocks().is_empty());
    }

    #[tokio::test]
    async fn test_l1_header_range_prefetches_full_requested_range() {
        let head = Header { number: 33, parent_hash: TEST_HASH, ..Default::default() };
        let head_hash = head.hash_slow();
        let mut encoded_head = Vec::new();
        head.encode(&mut encoded_head);

        let block = Header { number: 0, parent_hash: B256::ZERO, ..Default::default() };
        let block_hash = block.hash_slow();
        let mut encoded_block = Vec::new();
        block.encode(&mut encoded_block);
        let raw_block = Bytes::from(encoded_block);

        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(encoded_head));
        asserter.push_success(&raw_block);

        let l1 = provider_builder().connect_mocked_client(asserter);
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let providers = test_providers_with_l1(l1, l2);
        let prefetcher = L1HeaderPrefetcher::new(Arc::new(providers));
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));
        for number in 1..33 {
            assert!(prefetcher.mark_block_scheduled(number));
        }

        prefetcher.prefetch_range(Arc::clone(&kv), 0, 33, head_hash).await;

        let stored = kv.read().await.get(PreimageKey::new_keccak256(*block_hash).into()).unwrap();
        assert_eq!(stored, raw_block);
        assert!(!prefetcher.lock_scheduled_blocks().contains(&0));
    }

    #[tokio::test]
    async fn test_l1_header_range_skips_when_head_number_is_not_pinned_head() {
        let canonical_head = Header { number: 11, parent_hash: TEST_HASH, ..Default::default() };
        let mut encoded_head = Vec::new();
        canonical_head.encode(&mut encoded_head);

        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(encoded_head));
        let l1 = provider_builder().connect_mocked_client(asserter);
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let providers = test_providers_with_l1(l1, l2);
        let prefetcher = L1HeaderPrefetcher::new(Arc::new(providers));
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));

        prefetcher.prefetch_range(Arc::clone(&kv), 10, 11, B256::new([0x99; 32])).await;

        assert!(kv.read().await.get(PreimageKey::new_keccak256([0x99; 32]).into()).is_none());
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

    #[tokio::test]
    async fn test_l1_block_header_rejects_hash_mismatch() {
        let requested_hash = TEST_HASH;
        let header = Header { number: 7, parent_hash: B256::new([0x24; 32]), ..Default::default() };
        let actual_hash = header.hash_slow();
        let mut encoded_header = Vec::new();
        header.encode(&mut encoded_header);
        let raw_header = Bytes::from(encoded_header);
        let asserter = Asserter::new();
        asserter.push_success(&raw_header);
        let l1 = provider_builder().connect_mocked_client(asserter);
        let l2 = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let providers = test_providers_with_l1(l1, l2);
        let kv: SharedKeyValueStore = Arc::new(RwLock::new(MemoryKeyValueStore::new()));
        let hint = HintType::L1BlockHeader.with_data(&[requested_hash.as_slice()]);

        let err = handle_hint(hint, &test_cfg(), &providers, Arc::clone(&kv)).await.unwrap_err();

        match err {
            HostError::HeaderPreimageHashMismatch { expected, actual } => {
                assert_eq!(expected, requested_hash);
                assert_eq!(actual, actual_hash);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(kv.read().await.get(PreimageKey::new_keccak256(*requested_hash).into()).is_none());
        assert!(kv.read().await.get(PreimageKey::new_keccak256(*actual_hash).into()).is_none());
    }
}
