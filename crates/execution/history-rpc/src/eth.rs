//! Local-first, archive-on-miss `eth_` reads (TDD §5).
//!
//! Each method asks the node first. Only when the node answers "pruned" (EIP-4444 error) or
//! returns nothing for a block it no longer stores does the request go to the archive. Local
//! block, transaction and receipt answers are also replaced when the node's receipts for that
//! block are pruned, because Base fills deposit fields (`depositNonce`,
//! `depositReceiptVersion`) from receipts and defaults them when they are missing.
//!
//! State reads check up front whether the node still has the history domain the read needs:
//! account history for `getBalance`, `getTransactionCount` and `getCode`, storage history for
//! `getStorageAt`. reth's historical state provider refuses a read when either domain is
//! pruned at the block, so with unequal retention a read whose own domain is retained can still
//! fail; a local `StateAtBlockPruned` refusal also goes to the archive, while successful local
//! answers are kept. Block tags and hashes are resolved through the node's provider first, so
//! `earliest`, `safe` and `finalized` mean what they mean to the node. When the archive cannot
//! answer (`HistoryPruned`, `Unavailable`), the node's own answer is returned.

use std::{
    future::Future,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::{Filter, FilterBlockOption, error::EthRpcErrorCode};
use alloy_serde::JsonStorageKey;
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types::{
    BaseBlockResponse, BaseHeaderResponse, BaseLogResponse, BaseTransactionReceipt,
    BaseTransactionRequest, Transaction,
};
use base_execution_eip8130_rpc::Eip8130EthApiOverrideServer;
use base_history_store::{
    ArchiveLayout, ArchiveReader, ArchiveSnapshot, CatalogStore, HistoryStoreError, ReaderConfig,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{
        ErrorObjectOwned,
        error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE},
    },
};
use object_store::ObjectStore;
use reth_prune_types::PruneSegment;
use reth_rpc_eth_api::{EthApiServer, EthFilterApiServer};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::{
    BlockIdReader, BlockNumReader, PruneCheckpointReader, StateProvider, StateProviderFactory,
    errors::provider::ProviderError,
};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::{ArchiveConverter, ArchivedBlockWithReceipts};

/// Largest block range one `eth_getLogs` call may scan in the archive.
pub const MAX_ARCHIVE_LOG_RANGE: u64 = 10_000;

/// The archive-backed `eth_` methods.
#[rpc(server, namespace = "eth")]
pub trait ArchiveEthApi {
    /// `eth_getBlockByNumber`.
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<BaseBlockResponse>>;

    /// `eth_getBlockByHash`.
    #[method(name = "getBlockByHash")]
    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<BaseBlockResponse>>;

    /// `eth_getTransactionByHash`.
    #[method(name = "getTransactionByHash")]
    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>>;

    /// `eth_getTransactionReceipt`.
    #[method(name = "getTransactionReceipt")]
    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<BaseTransactionReceipt>>;

    /// `eth_getLogs`.
    #[method(name = "getLogs")]
    async fn logs(&self, filter: Filter) -> RpcResult<Vec<BaseLogResponse>>;

    /// `eth_getBalance`.
    #[method(name = "getBalance")]
    async fn balance(&self, address: Address, block: Option<BlockId>) -> RpcResult<U256>;

    /// `eth_getStorageAt`.
    #[method(name = "getStorageAt")]
    async fn storage_at(
        &self,
        address: Address,
        slot: JsonStorageKey,
        block: Option<BlockId>,
    ) -> RpcResult<B256>;

    /// `eth_getTransactionCount`, with the EIP-8130 `nonce_key` extension.
    #[method(name = "getTransactionCount")]
    async fn transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        nonce_key: Option<U256>,
    ) -> RpcResult<U256>;

    /// `eth_getCode`.
    #[method(name = "getCode")]
    async fn code(&self, address: Address, block: Option<BlockId>) -> RpcResult<Bytes>;
}

/// The node's `eth_` handler with Base response types.
pub trait LocalEthApi:
    EthApiServer<
        BaseTransactionRequest,
        Transaction,
        BaseBlockResponse,
        BaseTransactionReceipt,
        BaseHeaderResponse,
        BaseTxEnvelope,
    > + Send
    + Sync
    + 'static
{
}

impl<T> LocalEthApi for T where
    T: EthApiServer<
            BaseTransactionRequest,
            Transaction,
            BaseBlockResponse,
            BaseTransactionReceipt,
            BaseHeaderResponse,
            BaseTxEnvelope,
        > + Send
        + Sync
        + 'static
{
}

/// The node's `eth_getLogs` handler with Base response types.
pub trait LocalFilterApi:
    EthFilterApiServer<Transaction, BaseLogResponse> + Send + Sync + 'static
{
}

impl<T> LocalFilterApi for T where
    T: EthFilterApiServer<Transaction, BaseLogResponse> + Send + Sync + 'static
{
}

/// Where the reader finds the archive.
#[derive(Debug, Clone)]
pub struct ArchiveSource {
    /// Object store holding the archive.
    pub store: Arc<dyn ObjectStore>,
    /// Key layout.
    pub layout: ArchiveLayout,
}

/// Lazily opened archive reader.
///
/// Opening fails while the catalog is missing or unreachable; the node then keeps serving local
/// data, and archive-bound requests fail fast for [`ReaderConfig::snapshot_ttl`] before the
/// next one tries to open the reader again.
#[derive(Debug)]
pub struct ArchiveHandle {
    /// Archive location.
    pub source: ArchiveSource,
    /// Reader settings.
    pub config: ReaderConfig,
    /// The reader once opened.
    pub reader: OnceCell<ArchiveReader>,
    /// When opening the reader last failed.
    pub open_failed_at: Mutex<Option<Instant>>,
}

impl ArchiveHandle {
    /// Creates a handle that opens the reader on first use.
    pub fn new(source: ArchiveSource, config: ReaderConfig) -> Self {
        Self { source, config, reader: OnceCell::new(), open_failed_at: Mutex::new(None) }
    }

    /// Startup identity check (TDD §5): reads the catalog once and fails on a reachable
    /// catalog for another chain, genesis or format. A missing catalog or unreachable storage
    /// is not an error; the reader stays lazy and retries on the first archive-bound request.
    ///
    /// `state_retention` is the node's shortest account or storage history distance. Reads of
    /// state unchanged since the requested block come from the node's state at archive `H`,
    /// and `H` trails the node's head by up to a segment plus the writer's finality lag, so a
    /// retention under two segments is logged as a misconfiguration.
    pub async fn check_identity(
        &self,
        state_retention: Option<u64>,
    ) -> Result<(), HistoryStoreError> {
        let catalogs =
            CatalogStore::new(Arc::clone(&self.source.store), self.source.layout.clone());
        match catalogs.load().await {
            Ok(Some(current)) => {
                current.catalog.verify(self.config.chain_id, self.config.genesis_hash)?;
                let blocks_per_segment =
                    ArchiveSnapshot::from_catalog(&current.catalog)?.blocks_per_segment;
                if let Some(retention) = state_retention
                    && retention < 2 * blocks_per_segment
                {
                    warn!(
                        retention,
                        blocks_per_segment,
                        "node state history retention is under two archive segments; reads of \
                         unchanged state may find the node's state at archive H pruned"
                    );
                }
                Ok(())
            }
            Ok(None) => {
                warn!("archive catalog missing at startup; serving local data until it appears");
                Ok(())
            }
            Err(err) => {
                warn!(error = %err, "archive unreachable at startup; serving local data");
                Ok(())
            }
        }
    }

    /// Returns the reader, opening it if needed. After a failed open, fails with
    /// `Unavailable` without touching storage until [`ReaderConfig::snapshot_ttl`] has passed.
    pub async fn reader(&self) -> Result<&ArchiveReader, HistoryStoreError> {
        if let Some(reader) = self.reader.get() {
            return Ok(reader);
        }
        let ttl = self.config.snapshot_ttl;
        if let Some(at) = *self.open_failed_at.lock().unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < ttl
        {
            return Err(HistoryStoreError::Unavailable {
                reason: format!("archive reader failed to open; retrying after {ttl:?}"),
            });
        }
        let opened = self
            .reader
            .get_or_try_init(|| {
                ArchiveReader::open(
                    Arc::clone(&self.source.store),
                    self.source.layout.clone(),
                    self.config.clone(),
                )
            })
            .await;
        if opened.is_err() {
            *self.open_failed_at.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Instant::now());
        }
        opened
    }
}

/// The RPC boundary of archive reads: the one place archive errors become JSON-RPC errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveRpcBoundary;

impl ArchiveRpcBoundary {
    /// Converts an archive error. Missing history keeps the EIP-4444 code so clients see the
    /// same signal the node gives; anything else is an internal error and never data.
    pub fn error(err: HistoryStoreError) -> ErrorObjectOwned {
        match err {
            HistoryStoreError::HistoryPruned { .. } | HistoryStoreError::Unavailable { .. } => {
                ErrorObjectOwned::owned(
                    EthRpcErrorCode::PrunedHistory.code(),
                    err.to_string(),
                    None::<()>,
                )
            }
            // Same code and message as reth's `QueryExceedsMaxResults`.
            HistoryStoreError::TooManyLogs { .. } => {
                ErrorObjectOwned::owned(INVALID_PARAMS_CODE, err.to_string(), None::<()>)
            }
            err => ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, err.to_string(), None::<()>),
        }
    }

    /// Picks the answer for a state read the archive cannot serve. A node refusal of pruned
    /// state becomes the archive's EIP-4444 error rather than reth's internal
    /// `StateAtBlockPruned` error; a successful node answer is kept.
    pub fn state_miss<T>(local: RpcResult<T>, err: HistoryStoreError) -> RpcResult<T> {
        match (local, err) {
            (
                Ok(value),
                HistoryStoreError::HistoryPruned { .. } | HistoryStoreError::Unavailable { .. },
            ) => Ok(value),
            (_, err) => Err(Self::error(err)),
        }
    }

    /// Whether the node answered with the EIP-4444 "pruned history" error.
    pub fn is_pruned(err: &ErrorObjectOwned) -> bool {
        err.code() == EthRpcErrorCode::PrunedHistory.code()
    }

    /// Whether the node refused a state read at `number` because its historical state provider
    /// found that history pruned. reth converts `ProviderError::StateAtBlockPruned` into an
    /// internal error, so the node's answer is compared with that same conversion.
    pub fn is_state_pruned(err: &ErrorObjectOwned, number: u64) -> bool {
        let expected =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(number)));
        err.code() == expected.code() && err.message() == expected.message()
    }
}

/// History domain a state read needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDomain {
    /// Account history: balance, nonce, code.
    Account,
    /// Storage history: storage slots.
    Storage,
}

/// What the node still holds, read from its provider at most once per
/// [`RetentionCache::ttl`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalRetention {
    /// Earliest block whose body the node holds.
    pub earliest_body: u64,
    /// Highest block whose account history is pruned.
    pub account_history_pruned: Option<u64>,
    /// Highest block whose storage history is pruned.
    pub storage_history_pruned: Option<u64>,
    /// Highest block whose receipts are pruned.
    pub receipts_pruned: Option<u64>,
}

impl LocalRetention {
    /// Reads the node's earliest body and prune checkpoints. A failed read counts as "not
    /// pruned", which keeps the request local.
    pub fn read<P: BlockNumReader + PruneCheckpointReader>(provider: &P) -> Self {
        let pruned = |segment| {
            provider.get_prune_checkpoint(segment).ok().flatten().and_then(|c| c.block_number)
        };
        Self {
            earliest_body: provider.earliest_block_number().unwrap_or_default(),
            account_history_pruned: pruned(PruneSegment::AccountHistory),
            storage_history_pruned: pruned(PruneSegment::StorageHistory),
            receipts_pruned: pruned(PruneSegment::Receipts),
        }
    }

    /// Whether state at `block` can be outside the node's retention, so routing needs the
    /// retention at all. The head and forkchoice tags (and an omitted block, which means
    /// `latest`) are always retained; should the node still refuse one as pruned, that
    /// refusal routes the read to the archive.
    pub const fn may_be_pruned(block: Option<BlockId>) -> bool {
        !matches!(
            block,
            None | Some(BlockId::Number(
                BlockNumberOrTag::Latest
                    | BlockNumberOrTag::Pending
                    | BlockNumberOrTag::Safe
                    | BlockNumberOrTag::Finalized
            ))
        )
    }

    /// Whether the node lacks `domain` history at `number`.
    pub fn state_pruned(&self, number: u64, domain: StateDomain) -> bool {
        let pruned = match domain {
            StateDomain::Account => self.account_history_pruned,
            StateDomain::Storage => self.storage_history_pruned,
        };
        pruned.is_some_and(|pruned| number <= pruned)
    }

    /// Whether a local block, transaction or receipt answer for `number` is incomplete: the
    /// body is gone (reth's block cache may still answer) or the receipts that carry deposit
    /// fields are.
    pub fn answer_incomplete(&self, number: u64) -> bool {
        number < self.earliest_body || self.receipts_pruned.is_some_and(|pruned| number <= pruned)
    }

    /// Whether a local miss may be history the node pruned, so the archive is worth asking.
    /// A block number qualifies only below what the node fully holds, never above its head; a
    /// hash (`None`) only when the node pruned any bodies or receipts. Polling for a receipt
    /// that is not mined yet therefore never reaches the archive on an unpruned node.
    pub fn archive_may_hold(&self, number: Option<u64>) -> bool {
        number.map_or_else(
            || self.earliest_body > 0 || self.receipts_pruned.is_some(),
            |number| self.answer_incomplete(number),
        )
    }
}

/// The node's [`LocalRetention`], reused for `ttl` so the highest-volume RPCs do not open four
/// provider read transactions each. Prune checkpoints move once per prune run, and a block
/// pruned inside the TTL is still caught: the node refuses it as pruned, and that refusal goes
/// to the archive.
#[derive(Debug)]
pub struct RetentionCache {
    /// How long one read is reused.
    pub ttl: Duration,
    /// Last read and when it was taken.
    pub cached: Mutex<Option<(Instant, LocalRetention)>>,
}

impl RetentionCache {
    /// Creates an empty cache that reuses a read for `ttl`.
    pub const fn new(ttl: Duration) -> Self {
        Self { ttl, cached: Mutex::new(None) }
    }

    /// Returns the cached retention, or calls `read` and caches its result once the cached
    /// value is older than [`Self::ttl`].
    pub fn get(&self, read: impl FnOnce() -> LocalRetention) -> LocalRetention {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, retention)) = *cached
            && at.elapsed() < self.ttl
        {
            return retention;
        }
        let retention = read();
        *cached = Some((Instant::now(), retention));
        retention
    }
}

/// Local-first, archive-on-miss implementation of [`ArchiveEthApiServer`].
#[derive(Debug)]
pub struct ArchiveEth<Eth, F, N, P> {
    /// The node's `eth_` handler.
    pub eth: Eth,
    /// The node's filter handler.
    pub filter: F,
    /// EIP-8130 `eth_getTransactionCount` for non-zero nonce keys.
    pub eip8130: N,
    /// The node's provider, used for state at `H` and pruning checks.
    pub provider: P,
    /// Archive reader.
    pub archive: Arc<ArchiveHandle>,
    /// Response builder.
    pub converter: ArchiveConverter,
    /// Most logs one archive `eth_getLogs` response may hold (the node's
    /// `--rpc.max-logs-per-response`).
    pub max_logs_per_response: usize,
    /// The node's retention, reused for the archive's snapshot TTL.
    pub retention: RetentionCache,
}

impl<Eth, F, N, P> ArchiveEth<Eth, F, N, P>
where
    Eth: LocalEthApi,
    F: LocalFilterApi,
    N: Eip8130EthApiOverrideServer,
    P: StateProviderFactory
        + BlockIdReader
        + BlockNumReader
        + PruneCheckpointReader
        + Send
        + Sync
        + 'static,
{
    /// The node's retention, read through [`Self::retention`].
    pub fn local_retention(&self) -> LocalRetention {
        self.retention.get(|| LocalRetention::read(&self.provider))
    }

    /// Reads block `number` and its receipts from the archive.
    pub async fn archived_block(
        &self,
        reader: &ArchiveReader,
        snapshot: &ArchiveSnapshot,
        number: u64,
    ) -> Result<ArchivedBlockWithReceipts, HistoryStoreError> {
        let block = reader.block(snapshot, number).await?;
        let receipts = reader.receipts(snapshot, number).await?;
        Ok(self.converter.prepare(block, receipts))
    }

    /// Serves a block from the archive after a local miss; `local` is what the node returned.
    pub async fn block_fallback(
        &self,
        number: Option<u64>,
        hash: Option<B256>,
        full: bool,
        local: RpcResult<Option<BaseBlockResponse>>,
    ) -> RpcResult<Option<BaseBlockResponse>> {
        let retention = self.local_retention();
        match &local {
            Ok(Some(block)) if !retention.answer_incomplete(block.header.number) => return local,
            Ok(None) if !retention.archive_may_hold(number) => return local,
            Err(err) if !ArchiveRpcBoundary::is_pruned(err) => return local,
            _ => {}
        }
        let archived = async {
            let reader = self.archive.reader().await?;
            let snapshot = reader.snapshot().await?;
            let number = match (number, hash) {
                (Some(number), _) => Some(number),
                (None, Some(hash)) => reader.block_number(&snapshot, hash).await?,
                (None, None) => None,
            };
            let Some(number) = number else { return Ok(None) };
            Ok::<_, HistoryStoreError>(Some(self.archived_block(reader, &snapshot, number).await?))
        };
        match archived.await {
            Ok(Some(archived)) => Ok(Some(self.converter.block(&archived, full))),
            Ok(None) => local,
            Err(err) => Self::miss(local, err),
        }
    }

    /// Whether a local answer for `number` must be replaced by the archive's
    /// ([`LocalRetention::answer_incomplete`]); a pending answer (`None`) never is.
    pub fn local_answer_incomplete(&self, number: Option<u64>) -> bool {
        number.is_some_and(|number| self.local_retention().answer_incomplete(number))
    }

    /// Whether a transaction or receipt the node does not know may be in the archive
    /// ([`LocalRetention::archive_may_hold`]).
    pub fn archive_may_hold_hash(&self) -> bool {
        self.local_retention().archive_may_hold(None)
    }

    /// Picks the answer when the archive cannot serve a request the node missed: the node's
    /// own answer for missing history, the archive error for anything else.
    pub fn miss<T>(local: RpcResult<T>, err: HistoryStoreError) -> RpcResult<T> {
        match err {
            HistoryStoreError::HistoryPruned { .. } | HistoryStoreError::Unavailable { .. } => {
                debug!(error = %err, "archive cannot serve request");
                local
            }
            err => Err(ArchiveRpcBoundary::error(err)),
        }
    }

    /// Resolves the archived block and transaction index for a transaction hash.
    pub async fn archived_transaction(
        &self,
        hash: B256,
    ) -> Result<Option<(ArchivedBlockWithReceipts, usize)>, HistoryStoreError> {
        let reader = self.archive.reader().await?;
        let snapshot = reader.snapshot().await?;
        let Some((number, index)) = reader.transaction_location(&snapshot, hash).await? else {
            return Ok(None);
        };
        let archived = self.archived_block(reader, &snapshot, number).await?;
        Ok(Some((archived, index as usize)))
    }

    /// Serves a state read locally unless the node lacks its history. Returns `Ok` with the
    /// node's answer, or `Err(number)` when the archive must serve block `number`.
    ///
    /// The block id is resolved through the node's provider (`earliest` is the node's earliest
    /// block, `safe`/`finalized` its forkchoice). The archive serves the read when the prune
    /// checkpoint of the domain the read needs covers the block, without asking the node, or
    /// when the node refuses the read as pruned: reth's historical state provider checks both
    /// domains' checkpoints and the changeset static files, so unequal retention refuses reads
    /// whose own domain is retained, either with `StateAtBlockPruned` (an internal error) or
    /// with the EIP-4444 code (`InsufficientChangesets`, `BlockExpired`). Any other local answer, success or error, is returned as is.
    pub async fn local_state<T, Fut>(
        &self,
        block: Option<BlockId>,
        domain: StateDomain,
        local: impl FnOnce() -> Fut,
    ) -> Result<RpcResult<T>, u64>
    where
        Fut: Future<Output = RpcResult<T>>,
    {
        let number =
            block.and_then(|block| self.provider.block_number_for_id(block).ok().flatten());
        if let Some(number) = number.filter(|number| {
            LocalRetention::may_be_pruned(block)
                && self.local_retention().state_pruned(*number, domain)
        }) {
            return Err(number);
        }
        let answer = local().await;
        match (&answer, number) {
            (Err(err), Some(number))
                if ArchiveRpcBoundary::is_pruned(err)
                    || ArchiveRpcBoundary::is_state_pruned(err, number) =>
            {
                debug!(number, "node refused pruned state; serving from the archive");
                Err(number)
            }
            _ => Ok(answer),
        }
    }

    /// Resolves an account at a locally pruned block from the archive.
    pub async fn archived_account(
        &self,
        address: Address,
        number: u64,
    ) -> Result<
        (base_history_store::HistoryLookup<Option<reth_primitives_traits::Account>>, u64),
        HistoryStoreError,
    > {
        let reader = self.archive.reader().await?;
        let snapshot = reader.snapshot().await?;
        let lookup = reader.account_history(&snapshot, address, number).await?;
        Ok((lookup, snapshot.require_h()?))
    }

    /// Reads `read` from the node's state at `H` for keys unchanged since the requested block.
    /// When the node's `domain` history no longer reaches `H` (the archive lags the node's
    /// retention), the read is `Unavailable`, not an internal error.
    pub fn unchanged_state<T>(
        &self,
        h: u64,
        domain: StateDomain,
        read: impl FnOnce(&dyn StateProvider) -> Result<T, HistoryStoreError>,
    ) -> Result<T, HistoryStoreError> {
        let lagging = || HistoryStoreError::Unavailable {
            reason: format!("archive H {h} is older than the node's {domain:?} history retention"),
        };
        if self.local_retention().state_pruned(h, domain) {
            return Err(lagging());
        }
        let state = self.provider.history_by_block_number(h);
        match state.map_err(HistoryStoreError::from).and_then(|state| read(state.as_ref())) {
            Err(HistoryStoreError::StaticFile(
                ProviderError::StateAtBlockPruned(_) | ProviderError::InsufficientChangesets { .. },
            )) => Err(lagging()),
            other => other,
        }
    }

    /// Account at `number` from the archive plus the node's state at `H`.
    pub async fn account_at(
        &self,
        address: Address,
        number: u64,
    ) -> Result<Option<reth_primitives_traits::Account>, HistoryStoreError> {
        let (lookup, h) = self.archived_account(address, number).await?;
        self.unchanged_state(h, StateDomain::Account, |state| lookup.or_state(state, address))
    }

    /// Converts a block range bound to a number using the node's view of tags.
    pub fn resolve(&self, bound: Option<BlockNumberOrTag>) -> Result<u64, HistoryStoreError> {
        let best = self.provider.best_block_number()?;
        Ok(match bound {
            None => best,
            Some(tag) => self.provider.convert_block_number(tag)?.unwrap_or(best),
        })
    }

    /// Serves `eth_getLogs` after the node reported pruned history: `E..=H` from the archive,
    /// anything above `H` from the node.
    pub async fn archived_logs(
        &self,
        filter: Filter,
    ) -> Result<Vec<BaseLogResponse>, ErrorObjectOwned> {
        let reader = self.archive.reader().await.map_err(ArchiveRpcBoundary::error)?;
        let snapshot = reader.snapshot().await.map_err(ArchiveRpcBoundary::error)?;
        let (from, to) = match filter.block_option {
            FilterBlockOption::AtBlockHash(hash) => {
                match reader
                    .block_number(&snapshot, hash)
                    .await
                    .map_err(ArchiveRpcBoundary::error)?
                {
                    Some(number) => (number, number),
                    None => {
                        return Err(ArchiveRpcBoundary::error(HistoryStoreError::Unavailable {
                            reason: format!("block {hash} not in archive"),
                        }));
                    }
                }
            }
            FilterBlockOption::Range { from_block, to_block } => (
                self.resolve(from_block.or(Some(BlockNumberOrTag::Earliest)))
                    .map_err(ArchiveRpcBoundary::error)?,
                self.resolve(to_block).map_err(ArchiveRpcBoundary::error)?,
            ),
        };
        let split = snapshot.split(from, to);
        if let Some((start, _)) = split.pruned {
            return Err(ArchiveRpcBoundary::error(HistoryStoreError::HistoryPruned {
                block: start,
                earliest: snapshot.e,
            }));
        }
        let mut out = Vec::new();
        if let Some((start, end)) = split.archive {
            if end - start + 1 > MAX_ARCHIVE_LOG_RANGE {
                return Err(ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!("archive log range exceeds {MAX_ARCHIVE_LOG_RANGE} blocks"),
                    None::<()>,
                ));
            }
            let blocks = reader
                .logs(&snapshot, &filter, start, end, self.max_logs_per_response)
                .await
                .map_err(ArchiveRpcBoundary::error)?;
            for group in &blocks {
                let timestamp = group.block.header.timestamp;
                let timestamp_ms = self.converter.timestamp_ms(&group.block);
                out.extend(
                    group
                        .logs
                        .iter()
                        .map(|log| ArchiveConverter::log(log, timestamp, timestamp_ms)),
                );
            }
        }
        if let Some((start, end)) = split.local {
            let local = filter.from_block(start).to_block(end);
            out.extend(EthFilterApiServer::logs(&self.filter, local).await?);
        }
        Ok(out)
    }
}

#[async_trait]
impl<Eth, F, N, P> ArchiveEthApiServer for ArchiveEth<Eth, F, N, P>
where
    Eth: LocalEthApi,
    F: LocalFilterApi,
    N: Eip8130EthApiOverrideServer,
    P: StateProviderFactory
        + BlockIdReader
        + BlockNumReader
        + PruneCheckpointReader
        + Send
        + Sync
        + 'static,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<BaseBlockResponse>> {
        let local = EthApiServer::block_by_number(&self.eth, number, full).await;
        let BlockNumberOrTag::Number(n) = number else { return local };
        self.block_fallback(Some(n), None, full, local).await
    }

    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<BaseBlockResponse>> {
        let local = EthApiServer::block_by_hash(&self.eth, hash, full).await;
        self.block_fallback(None, Some(hash), full, local).await
    }

    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>> {
        let local = EthApiServer::transaction_by_hash(&self.eth, hash).await;
        match &local {
            Ok(Some(tx)) if !self.local_answer_incomplete(tx.block_number) => return local,
            Ok(None) if !self.archive_may_hold_hash() => return local,
            Err(err) if !ArchiveRpcBoundary::is_pruned(err) => return local,
            _ => {}
        }
        match self.archived_transaction(hash).await {
            Ok(Some((archived, index))) => Ok(self.converter.transaction(&archived, index)),
            Ok(None) => local,
            Err(err) => Self::miss(local, err),
        }
    }

    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<BaseTransactionReceipt>> {
        let local = EthApiServer::transaction_receipt(&self.eth, hash).await;
        match &local {
            Ok(Some(receipt)) if !self.local_answer_incomplete(receipt.inner.block_number) => {
                return local;
            }
            Ok(None) if !self.archive_may_hold_hash() => return local,
            Err(err) if !ArchiveRpcBoundary::is_pruned(err) => return local,
            _ => {}
        }
        match self.archived_transaction(hash).await {
            Ok(Some((archived, index))) => {
                self.converter.receipt(&archived, index).map_err(ArchiveRpcBoundary::error)
            }
            Ok(None) => local,
            Err(err) => Self::miss(local, err),
        }
    }

    async fn logs(&self, filter: Filter) -> RpcResult<Vec<BaseLogResponse>> {
        match EthFilterApiServer::logs(&self.filter, filter.clone()).await {
            Err(err) if ArchiveRpcBoundary::is_pruned(&err) => self.archived_logs(filter).await,
            local => local,
        }
    }

    async fn balance(&self, address: Address, block: Option<BlockId>) -> RpcResult<U256> {
        let number = match self
            .local_state(block, StateDomain::Account, || {
                EthApiServer::balance(&self.eth, address, block)
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        match self.account_at(address, number).await {
            Ok(account) => Ok(account.map(|account| account.balance).unwrap_or_default()),
            Err(err) => ArchiveRpcBoundary::state_miss(
                EthApiServer::balance(&self.eth, address, block).await,
                err,
            ),
        }
    }

    async fn storage_at(
        &self,
        address: Address,
        slot: JsonStorageKey,
        block: Option<BlockId>,
    ) -> RpcResult<B256> {
        let number = match self
            .local_state(block, StateDomain::Storage, || {
                EthApiServer::storage_at(&self.eth, address, slot, block)
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        let key = slot.as_b256();
        let value = async {
            let reader = self.archive.reader().await?;
            let snapshot = reader.snapshot().await?;
            let lookup = reader.storage_history(&snapshot, address, key, number).await?;
            let h = snapshot.require_h()?;
            self.unchanged_state(h, StateDomain::Storage, |state| {
                lookup.or_state(state, address, key)
            })
        }
        .await;
        match value {
            Ok(value) => Ok(B256::from(value)),
            Err(err) => ArchiveRpcBoundary::state_miss(
                EthApiServer::storage_at(&self.eth, address, slot, block).await,
                err,
            ),
        }
    }

    async fn transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        nonce_key: Option<U256>,
    ) -> RpcResult<U256> {
        if nonce_key.is_some_and(|key| !key.is_zero()) {
            return Eip8130EthApiOverrideServer::get_transaction_count(
                &self.eip8130,
                address,
                block,
                nonce_key,
            )
            .await;
        }
        let number = match self
            .local_state(block, StateDomain::Account, || {
                EthApiServer::transaction_count(&self.eth, address, block)
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        match self.account_at(address, number).await {
            Ok(account) => Ok(U256::from(account.map(|account| account.nonce).unwrap_or_default())),
            Err(err) => ArchiveRpcBoundary::state_miss(
                EthApiServer::transaction_count(&self.eth, address, block).await,
                err,
            ),
        }
    }

    async fn code(&self, address: Address, block: Option<BlockId>) -> RpcResult<Bytes> {
        let number = match self
            .local_state(block, StateDomain::Account, || {
                EthApiServer::get_code(&self.eth, address, block)
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        let code = async {
            let (lookup, h) = self.archived_account(address, number).await?;
            self.unchanged_state(h, StateDomain::Account, |state| {
                lookup.code_or_state(state, address)
            })
        }
        .await;
        match code {
            Ok(code) => Ok(code),
            Err(err) => ArchiveRpcBoundary::state_miss(
                EthApiServer::get_code(&self.eth, address, block).await,
                err,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review5 P2 retention reads: state reads at the head and forkchoice tags never read the
    /// retention; explicit numbers, hashes and `earliest` do.
    #[test]
    fn recent_tags_skip_the_retention_read() {
        for tag in [
            BlockNumberOrTag::Latest,
            BlockNumberOrTag::Pending,
            BlockNumberOrTag::Safe,
            BlockNumberOrTag::Finalized,
        ] {
            assert!(!LocalRetention::may_be_pruned(Some(BlockId::Number(tag))), "{tag:?} skips");
        }
        assert!(!LocalRetention::may_be_pruned(None), "an omitted block means latest");
        assert!(LocalRetention::may_be_pruned(Some(BlockId::number(5))), "a number reads it");
        assert!(
            LocalRetention::may_be_pruned(Some(BlockId::Number(BlockNumberOrTag::Earliest))),
            "earliest reads it"
        );
        assert!(LocalRetention::may_be_pruned(Some(BlockId::hash(B256::ZERO))), "a hash reads it");
    }

    /// Review5 P2 retention reads: every call inside the TTL reuses one provider read, and the
    /// first call after the TTL reads again and sees the new checkpoints.
    #[test]
    fn retention_is_read_once_per_ttl() {
        let cache = RetentionCache::new(Duration::from_millis(100));
        let reads = std::cell::Cell::new(0u64);
        let read = |earliest_body| {
            reads.set(reads.get() + 1);
            LocalRetention { earliest_body, ..Default::default() }
        };
        for call in 0..50 {
            assert_eq!(cache.get(|| read(10)).earliest_body, 10, "call {call} sees the first read");
        }
        assert_eq!(reads.get(), 1, "50 calls inside the TTL make exactly one provider read");
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(cache.get(|| read(20)).earliest_body, 20, "a call after the TTL reads again");
        assert_eq!(cache.get(|| read(30)).earliest_body, 20, "the refreshed read is reused");
        assert_eq!(reads.get(), 2, "exactly one refresh after the TTL");
    }

    /// Only the domain a read needs decides the route: pruned storage history leaves account
    /// reads local and vice versa.
    #[test]
    fn state_routing_is_per_domain() {
        let storage_only =
            LocalRetention { storage_history_pruned: Some(100), ..Default::default() };
        assert!(!storage_only.state_pruned(50, StateDomain::Account), "account history is local");
        assert!(
            storage_only.state_pruned(50, StateDomain::Storage),
            "storage history is pruned at 50"
        );
        assert!(
            storage_only.state_pruned(100, StateDomain::Storage),
            "checkpoint block itself is pruned"
        );
        assert!(
            !storage_only.state_pruned(101, StateDomain::Storage),
            "101 is above the checkpoint"
        );
        let account_only =
            LocalRetention { account_history_pruned: Some(100), ..Default::default() };
        assert!(
            account_only.state_pruned(50, StateDomain::Account),
            "account history is pruned at 50"
        );
        assert!(!account_only.state_pruned(50, StateDomain::Storage), "storage history is local");
    }

    /// A local answer is incomplete when the body is gone or, with bodies retained, when only
    /// the receipts carrying deposit fields are pruned.
    #[test]
    fn receipt_only_pruning_makes_local_answers_incomplete() {
        let receipts_only =
            LocalRetention { earliest_body: 0, receipts_pruned: Some(40), ..Default::default() };
        assert!(receipts_only.answer_incomplete(40), "body local but receipts pruned at 40");
        assert!(!receipts_only.answer_incomplete(41), "receipts kept from 41");
        let bodies = LocalRetention { earliest_body: 10, ..Default::default() };
        assert!(bodies.answer_incomplete(9), "body pruned below 10");
        assert!(!bodies.answer_incomplete(10), "body and receipts local at 10");
    }

    /// A node refusal of pruned state is recognised through reth's own conversion, for the
    /// requested block only; other internal errors are not.
    #[test]
    fn state_pruned_refusal_is_recognised() {
        let refusal =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(7)));
        assert!(ArchiveRpcBoundary::is_state_pruned(&refusal, 7), "refusal at the requested block");
        assert!(!ArchiveRpcBoundary::is_state_pruned(&refusal, 8), "a refusal for another block");
        let other = ErrorObjectOwned::from(EthApiError::from(ProviderError::BestBlockNotFound));
        assert!(!ArchiveRpcBoundary::is_state_pruned(&other, 7), "another internal error");
    }

    /// A miss goes to the archive only for history the node pruned: never for a block above
    /// what the node fully holds, and for hashes only once the node pruned bodies or receipts.
    #[test]
    fn misses_reach_the_archive_only_for_pruned_history() {
        let unpruned = LocalRetention::default();
        assert!(!unpruned.archive_may_hold(None), "unknown hash on an unpruned node stays local");
        assert!(!unpruned.archive_may_hold(Some(1_000)), "head + 1 on an unpruned node");
        let bodies = LocalRetention { earliest_body: 10, ..Default::default() };
        assert!(bodies.archive_may_hold(None), "a hash may name a pruned block");
        assert!(bodies.archive_may_hold(Some(9)), "block 9 is pruned");
        assert!(!bodies.archive_may_hold(Some(10)), "block 10 is local");
        assert!(!bodies.archive_may_hold(Some(1_000)), "head + 1 never goes to the archive");
        let receipts = LocalRetention { receipts_pruned: Some(5), ..Default::default() };
        assert!(receipts.archive_may_hold(None), "receipt pruning alone qualifies hashes");
        assert!(receipts.archive_may_hold(Some(5)), "receipts pruned at 5");
        assert!(!receipts.archive_may_hold(Some(6)), "receipts kept from 6");
    }

    /// A state read the archive cannot complete because the node's state at `H` is pruned
    /// answers with the EIP-4444 code, not reth's internal `StateAtBlockPruned` error; a
    /// successful node answer is kept and other archive failures stay internal.
    #[test]
    fn unresolvable_state_reads_are_pruned_history() {
        let refusal: RpcResult<U256> =
            Err(ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(1))));
        assert_eq!(refusal.as_ref().unwrap_err().code(), INTERNAL_ERROR_CODE, "reth's refusal");
        let lagging = || HistoryStoreError::Unavailable { reason: "archive H 15".to_string() };
        let answer = ArchiveRpcBoundary::state_miss(refusal, lagging()).unwrap_err();
        assert_eq!(answer.code(), EthRpcErrorCode::PrunedHistory.code(), "becomes 4444");
        let kept = ArchiveRpcBoundary::state_miss(Ok(U256::from(7)), lagging());
        assert_eq!(kept.unwrap(), U256::from(7), "a node answer is kept");
        let corrupt = ArchiveRpcBoundary::state_miss(
            Ok(U256::from(7)),
            HistoryStoreError::Index { reason: "bad".to_string() },
        );
        assert_eq!(corrupt.unwrap_err().code(), INTERNAL_ERROR_CODE, "corruption is internal");
    }

    /// The archive log cap keeps reth's `QueryExceedsMaxResults` code and message.
    #[test]
    fn too_many_logs_matches_reth() {
        let err = ArchiveRpcBoundary::error(HistoryStoreError::TooManyLogs {
            max: 2,
            from: 100,
            to: 101,
        });
        assert_eq!(err.code(), INVALID_PARAMS_CODE, "invalid params, as reth");
        assert_eq!(
            err.message(),
            "query exceeds max results 2, retry with the range 100-101",
            "reth's message"
        );
    }

    /// Only missing-history archive errors keep the EIP-4444 code.
    #[test]
    fn archive_errors_map_once_at_the_boundary() {
        let pruned =
            ArchiveRpcBoundary::error(HistoryStoreError::HistoryPruned { block: 1, earliest: 8 });
        assert_eq!(pruned.code(), EthRpcErrorCode::PrunedHistory.code(), "pruned keeps 4444");
        assert!(ArchiveRpcBoundary::is_pruned(&pruned), "recognised as pruned");
        let corrupt = ArchiveRpcBoundary::error(HistoryStoreError::CorruptSegment {
            segment: 0,
            file: "x".to_string(),
            reason: "y".to_string(),
        });
        assert_eq!(corrupt.code(), INTERNAL_ERROR_CODE, "corruption is internal, never data");
        assert!(!ArchiveRpcBoundary::is_pruned(&corrupt), "not pruned");
    }
}
