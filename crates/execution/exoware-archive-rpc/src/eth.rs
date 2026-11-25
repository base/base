//! Local-first, archive-on-prune `eth_` reads.
//!
//! Each method asks the node first and goes to the archive only when the node's own data for
//! the request is pruned, routed per method by the data it needs: bodies and receipts for
//! block, transaction and receipt reads (Base fills deposit fields from receipts and defaults
//! them when they are missing, so a local success over pruned receipts is replaced), account
//! history for balance, nonce and code, storage history for storage slots, and receipts for
//! logs, which are split into an archive part and a local part with no gap.
//!
//! Errors: a number-addressed request in the pruned range, or a hash that resolved in the
//! archive, gets JSON-RPC 4444 when the archive cannot answer it (outage, timeout, integrity
//! fault, retention moved). `null` is returned only for a genuine miss, or when the store
//! failed before a hash the node does not know resolved (so pending-transaction polling does
//! not turn into errors during an outage).

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::{Filter, FilterBlockOption, Index, error::EthRpcErrorCode};
use alloy_serde::JsonStorageKey;
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types::{
    BaseBlockResponse, BaseHeaderResponse, BaseLogResponse, BaseTransactionReceipt,
    BaseTransactionRequest, Transaction,
};
use base_execution_base_tx_rpc::TxBaseEthApiOverrideServer;
use base_exoware_archive::{ArchiveError, LogFilter};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE},
};
use reth_rpc_eth_api::{EngineEthFilter, EthApiServer, QueryLimits};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::errors::provider::ProviderError;
use tracing::debug;

use crate::{
    ArchiveConverter, ArchiveHandle, ArchiveProvider, HashLookup, LocalRetention, PreparedBlock,
    RetentionCache, StateDomain,
};

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

    /// `eth_getBlockTransactionCountByNumber`.
    #[method(name = "getBlockTransactionCountByNumber")]
    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>>;

    /// `eth_getBlockTransactionCountByHash`.
    #[method(name = "getBlockTransactionCountByHash")]
    async fn block_transaction_count_by_hash(&self, hash: B256) -> RpcResult<Option<U256>>;

    /// `eth_getTransactionByHash`.
    #[method(name = "getTransactionByHash")]
    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>>;

    /// `eth_getTransactionByBlockHashAndIndex`.
    #[method(name = "getTransactionByBlockHashAndIndex")]
    async fn transaction_by_block_hash_and_index(
        &self,
        hash: B256,
        index: Index,
    ) -> RpcResult<Option<Transaction>>;

    /// `eth_getTransactionByBlockNumberAndIndex`.
    #[method(name = "getTransactionByBlockNumberAndIndex")]
    async fn transaction_by_block_number_and_index(
        &self,
        number: BlockNumberOrTag,
        index: Index,
    ) -> RpcResult<Option<Transaction>>;

    /// `eth_getTransactionReceipt`.
    #[method(name = "getTransactionReceipt")]
    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<BaseTransactionReceipt>>;

    /// `eth_getBlockReceipts`.
    #[method(name = "getBlockReceipts")]
    async fn block_receipts(&self, block: BlockId) -> RpcResult<Option<Vec<BaseTransactionReceipt>>>;

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

    /// `eth_getTransactionCount`, with the Base `nonce_key` extension.
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

/// The RPC boundary: the one place archive errors become JSON-RPC errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveRpcBoundary;

impl ArchiveRpcBoundary {
    /// Message prefix of archive 4444 errors.
    pub const PRUNED: &'static str = "pruned history unavailable";

    /// Converts an archive error: log-limit violations are invalid params, everything else is
    /// EIP-4444 "pruned history unavailable" with the reason, never data.
    pub fn error(err: ArchiveError) -> ErrorObjectOwned {
        match err {
            ArchiveError::LimitExceeded { .. } => {
                ErrorObjectOwned::owned(INVALID_PARAMS_CODE, err.to_string(), None::<()>)
            }
            err => ErrorObjectOwned::owned(
                EthRpcErrorCode::PrunedHistory.code(),
                format!("{}: {err}", Self::PRUNED),
                None::<()>,
            ),
        }
    }

    /// reth's `QueryExceedsMaxResults` error, same code and message.
    pub fn too_many_logs(max: usize, from: u64, to: u64) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            format!("query exceeds max results {max}, retry with the range {from}-{to}"),
            None::<()>,
        )
    }

    /// reth's `QueryExceedsMaxBlocks` error, same code and message.
    pub fn too_many_blocks(max: u64) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            format!("query exceeds max block range {max}"),
            None::<()>,
        )
    }

    /// Applies reth's `--rpc.max-logs-per-response` check to a combined result: a multi-block
    /// query whose logs exceed `max` fails at the first block that crosses it, suggesting the
    /// range up to the block before (or just `from` when `from` itself crosses).
    pub fn check_max_logs(
        max: Option<usize>,
        logs: &[BaseLogResponse],
        from: u64,
        to: u64,
    ) -> RpcResult<()> {
        let Some(max) = max else { return Ok(()) };
        if from == to || logs.len() <= max {
            return Ok(());
        }
        let crossing = logs[max].inner.block_number.unwrap_or(from);
        let retry_to = if crossing == from { from } else { crossing - 1 };
        Err(Self::too_many_logs(max, from, retry_to))
    }

    /// Whether the node answered with the EIP-4444 "pruned history" error.
    pub fn is_pruned(err: &ErrorObjectOwned) -> bool {
        err.code() == EthRpcErrorCode::PrunedHistory.code()
    }

    /// Whether the node refused a state read at `number` because its historical state
    /// provider found that history pruned (reth renders `StateAtBlockPruned` as an internal
    /// error).
    pub fn is_state_pruned(err: &ErrorObjectOwned, number: u64) -> bool {
        let expected =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(number)));
        err.code() == expected.code() && err.message() == expected.message()
    }

    /// Picks the answer of a hash-addressed request after the node missed: the archive's
    /// answer, the node's answer for a genuine miss or an unresolved outage, else 4444.
    pub fn by_hash<T>(local: RpcResult<Option<T>>, archived: HashLookup<T>) -> RpcResult<Option<T>> {
        match archived {
            HashLookup::Found(found) => Ok(Some(found)),
            HashLookup::Miss | HashLookup::Unresolved(_) => local,
            HashLookup::Failed(err) => Err(Self::error(err)),
        }
    }
}

/// Local-first, archive-on-prune implementation of [`ArchiveEthApiServer`].
#[derive(Debug)]
pub struct ArchiveEth<Eth, F, N, P> {
    /// The node's `eth_` handler.
    pub eth: Eth,
    /// The node's filter handler, called with explicit limits.
    pub filter: F,
    /// The Base transaction `eth_getTransactionCount` override, which this boundary keeps
    /// serving every local `eth_getTransactionCount` call through.
    pub base_tx: N,
    /// The node's provider.
    pub provider: P,
    /// Archive reader.
    pub archive: Arc<ArchiveHandle<P>>,
    /// Response builder.
    pub converter: ArchiveConverter,
    /// The node's `--rpc.max-blocks-per-filter` and `--rpc.max-logs-per-response`.
    pub limits: QueryLimits,
    /// The node's retention.
    pub retention: RetentionCache,
}

impl<Eth, F, N, P> ArchiveEth<Eth, F, N, P>
where
    Eth: LocalEthApi,
    F: EngineEthFilter<BaseLogResponse>,
    N: TxBaseEthApiOverrideServer,
    P: ArchiveProvider,
{
    /// The node's retention, read through [`Self::retention`].
    pub fn local_retention(&self) -> LocalRetention {
        self.retention.get(|| LocalRetention::read(&self.provider))
    }

    /// Whether a local answer about block `number` must go to the archive: `found` is the
    /// block number of a local success, `requested` the number a miss or refusal was for.
    pub fn needs_archive<T>(
        &self,
        local: &RpcResult<Option<T>>,
        found: impl FnOnce(&T) -> Option<u64>,
        requested: Option<u64>,
    ) -> bool {
        let retention = self.local_retention();
        match local {
            Ok(Some(value)) => found(value).is_some_and(|number| retention.answer_incomplete(number)),
            Ok(None) => retention.archive_may_hold(requested),
            Err(err) => ArchiveRpcBoundary::is_pruned(err),
        }
    }

    /// Reads and prepares block `number` from the archive.
    pub async fn archived_block(&self, number: u64) -> Result<PreparedBlock, ArchiveError> {
        self.archive
            .bounded(async {
                let reader = self.archive.reader().await?;
                let request = reader.begin().await?;
                let result = reader.block_in(&request, number).await;
                reader.conclude(&request, result).await
            })
            .await
            .map(|block| self.converter.prepare(block))
    }

    /// Resolves a block hash in the archive and reads the block.
    pub async fn archived_block_by_hash(&self, hash: B256) -> HashLookup<PreparedBlock> {
        let mut resolved = false;
        let outcome = self
            .archive
            .bounded(async {
                let reader = self.archive.reader().await?;
                let request = reader.begin().await?;
                let result = async {
                    let Some(number) = reader.block_number_by_hash_in(&request, hash).await? else {
                        return Ok(None);
                    };
                    resolved = true;
                    reader.block_in(&request, number).await.map(Some)
                }
                .await;
                reader.conclude(&request, result).await
            })
            .await;
        ArchiveHandle::<P>::classify(outcome.map(|b| b.map(|b| self.converter.prepare(b))), resolved)
    }

    /// Resolves a transaction hash in the archive and reads its block.
    pub async fn archived_transaction(&self, hash: B256) -> HashLookup<(PreparedBlock, usize)> {
        let mut resolved = false;
        let outcome = self
            .archive
            .bounded(async {
                let reader = self.archive.reader().await?;
                let request = reader.begin().await?;
                let result = async {
                    let Some(tx) = reader.transaction_by_hash_in(&request, hash).await? else {
                        return Ok(None);
                    };
                    resolved = true;
                    let block = reader.block_in(&request, tx.block_number).await?;
                    Ok(Some((block, tx.index as usize)))
                }
                .await;
                reader.conclude(&request, result).await
            })
            .await;
        ArchiveHandle::<P>::classify(
            outcome.map(|found| found.map(|(block, index)| (self.converter.prepare(block), index))),
            resolved,
        )
    }

    /// A block-level answer for `number` after a local miss: the archive's, or 4444.
    pub async fn block_answer<T>(
        &self,
        number: u64,
        build: impl FnOnce(&PreparedBlock) -> Result<Option<T>, ArchiveError>,
    ) -> RpcResult<Option<T>> {
        self.archived_block(number)
            .await
            .and_then(|block| build(&block))
            .map_err(ArchiveRpcBoundary::error)
    }

    /// A block-level answer for `hash` after a local miss.
    pub async fn block_answer_by_hash<T>(
        &self,
        hash: B256,
        local: RpcResult<Option<T>>,
        build: impl FnOnce(&PreparedBlock) -> Result<Option<T>, ArchiveError>,
    ) -> RpcResult<Option<T>> {
        let archived = match self.archived_block_by_hash(hash).await {
            HashLookup::Found(block) => match build(&block) {
                Ok(Some(answer)) => HashLookup::Found(answer),
                Ok(None) => HashLookup::Miss,
                Err(err) => HashLookup::Failed(err),
            },
            HashLookup::Miss => HashLookup::Miss,
            HashLookup::Unresolved(err) => HashLookup::Unresolved(err),
            HashLookup::Failed(err) => HashLookup::Failed(err),
        };
        ArchiveRpcBoundary::by_hash(local, archived)
    }

    /// Serves a state read locally unless the node lacks its history. Returns `Ok` with the
    /// node's answer, or `Err(number)` when the archive must serve block `number`: the prune
    /// checkpoint of the read's domain covers the block, or the node refused the read as
    /// pruned (reth checks both domains, so unequal retention can refuse a read whose own
    /// domain is retained).
    pub async fn local_state<T>(
        &self,
        block: Option<BlockId>,
        domain: StateDomain,
        local: impl AsyncFnOnce() -> RpcResult<T>,
    ) -> Result<RpcResult<T>, u64> {
        let number = LocalRetention::may_be_pruned(block)
            .then(|| block.and_then(|block| self.provider.block_number_for_id(block).ok().flatten()))
            .flatten();
        if let Some(number) = number.filter(|n| self.local_retention().state_pruned(*n, domain)) {
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

    /// Runs one state read against the archive, mapping errors to 4444.
    pub async fn archived_state<T>(
        &self,
        read: impl AsyncFnOnce(
            &crate::NodeArchiveReader<P>,
            &base_exoware_archive::ArchiveRequest<base_exoware_archive::ExowareSession>,
        ) -> Result<T, ArchiveError>,
    ) -> RpcResult<T> {
        self.archive
            .bounded(async {
                let reader = self.archive.reader().await?;
                let request = reader.begin().await?;
                let result = read(reader, &request).await;
                reader.conclude(&request, result).await
            })
            .await
            .map_err(ArchiveRpcBoundary::error)
    }

    /// Converts a block range bound using the node's view of tags; an omitted bound or one
    /// the node cannot resolve means the head, as in reth.
    pub fn resolve(&self, bound: Option<BlockNumberOrTag>, best: u64) -> RpcResult<u64> {
        let Some(bound) = bound.filter(|b| !b.is_pending()) else { return Ok(best) };
        Ok(self
            .provider
            .convert_block_number(bound)
            .map_err(|err| ErrorObjectOwned::from(EthApiError::from(err)))?
            .unwrap_or(best))
    }

    /// The archive's [`LogFilter`] for `filter` over `[from, to]`.
    pub fn log_filter(filter: &Filter, from: u64, to: u64) -> LogFilter {
        LogFilter {
            from,
            to,
            addresses: filter.address.iter().copied().collect(),
            topics: std::array::from_fn(|i| filter.topics[i].iter().copied().collect()),
        }
    }

    /// Serves `eth_getLogs` after the node reported pruned history: blocks below the node's
    /// first complete block from the archive, the rest from the node, with reth's limits over
    /// the whole range.
    pub async fn archived_logs(&self, filter: Filter) -> RpcResult<Vec<BaseLogResponse>> {
        let best = self
            .provider
            .best_block_number()
            .map_err(|err| ErrorObjectOwned::from(EthApiError::from(err)))?;
        let (from, to) = match filter.block_option {
            FilterBlockOption::AtBlockHash(hash) => {
                let local = self
                    .provider
                    .block_number(hash)
                    .map_err(|err| ErrorObjectOwned::from(EthApiError::from(err)))?;
                let number = match local {
                    Some(number) => number,
                    None => match self.archived_block_by_hash(hash).await {
                        HashLookup::Found(block) => block.block.header().number,
                        HashLookup::Miss | HashLookup::Unresolved(_) => {
                            return Err(EthApiError::from(ProviderError::HeaderNotFound(hash.into())).into());
                        }
                        HashLookup::Failed(err) => return Err(ArchiveRpcBoundary::error(err)),
                    },
                };
                (number, number)
            }
            FilterBlockOption::Range { from_block, to_block } => {
                (self.resolve(from_block, best)?, self.resolve(to_block, best)?)
            }
        };
        if from > to {
            return Err(ErrorObjectOwned::owned(INVALID_PARAMS_CODE, "invalid block range params", None::<()>));
        }
        if let Some(max) = self.limits.max_blocks_per_filter.filter(|max| to - from > *max) {
            return Err(ArchiveRpcBoundary::too_many_blocks(max));
        }
        let first_local = self.local_retention().first_complete();
        let mut out = Vec::new();
        if from < first_local {
            let archive_to = to.min(first_local - 1);
            let log_filter = Self::log_filter(&filter, from, archive_to);
            let logs = self
                .archive
                .bounded(async {
                    let reader = self.archive.reader().await?;
                    let request = reader.begin().await?;
                    let result = reader.logs_in(&request, &log_filter).await;
                    reader.conclude(&request, result).await
                })
                .await
                .map_err(ArchiveRpcBoundary::error)?;
            out.extend(logs.iter().map(ArchiveConverter::log));
            ArchiveRpcBoundary::check_max_logs(self.limits.max_logs_per_response, &out, from, to)?;
        }
        if to >= first_local {
            let local = filter.clone().from_block(from.max(first_local)).to_block(to);
            out.extend(EngineEthFilter::logs(&self.filter, local, QueryLimits::no_limits()).await?);
        }
        ArchiveRpcBoundary::check_max_logs(self.limits.max_logs_per_response, &out, from, to)?;
        Ok(out)
    }
}

#[async_trait]
impl<Eth, F, N, P> ArchiveEthApiServer for ArchiveEth<Eth, F, N, P>
where
    Eth: LocalEthApi,
    F: EngineEthFilter<BaseLogResponse>,
    N: TxBaseEthApiOverrideServer,
    P: ArchiveProvider,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<BaseBlockResponse>> {
        let local = EthApiServer::block_by_number(&self.eth, number, full).await;
        let Some(n) = number.as_number() else { return local };
        if !self.needs_archive(&local, |b| Some(b.header.number), Some(n)) {
            return local;
        }
        self.block_answer(n, |block| Ok(Some(self.converter.block(block, full)))).await
    }

    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<BaseBlockResponse>> {
        let local = EthApiServer::block_by_hash(&self.eth, hash, full).await;
        if !self.needs_archive(&local, |b| Some(b.header.number), None) {
            return local;
        }
        let local_number = self.provider.block_number(hash).ok().flatten();
        match local_number {
            Some(n) => self.block_answer(n, |block| Ok(Some(self.converter.block(block, full)))).await,
            None => {
                self.block_answer_by_hash(hash, local, |block| Ok(Some(self.converter.block(block, full))))
                    .await
            }
        }
    }

    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        let local = EthApiServer::block_transaction_count_by_number(&self.eth, number).await;
        let Some(n) = number.as_number() else { return local };
        let earliest_body = self.local_retention().earliest_body;
        let pruned = match &local {
            Ok(Some(_)) => false,
            Ok(None) => n < earliest_body,
            Err(err) => ArchiveRpcBoundary::is_pruned(err),
        };
        if !pruned {
            return local;
        }
        self.block_answer(n, |block| Ok(Some(U256::from(block.block.body().transactions.len())))).await
    }

    async fn block_transaction_count_by_hash(&self, hash: B256) -> RpcResult<Option<U256>> {
        let local = EthApiServer::block_transaction_count_by_hash(&self.eth, hash).await;
        let retention = self.local_retention();
        let pruned = match &local {
            Ok(Some(_)) => false,
            Ok(None) => retention.earliest_body > 0,
            Err(err) => ArchiveRpcBoundary::is_pruned(err),
        };
        if !pruned {
            return local;
        }
        let count = |block: &PreparedBlock| Ok(Some(U256::from(block.block.body().transactions.len())));
        match self.provider.block_number(hash).ok().flatten() {
            Some(n) if n < retention.earliest_body => self.block_answer(n, count).await,
            Some(_) => local,
            None => self.block_answer_by_hash(hash, local, count).await,
        }
    }

    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>> {
        let local = EthApiServer::transaction_by_hash(&self.eth, hash).await;
        if !self.needs_archive(&local, |tx| tx.block_number, None) {
            return local;
        }
        let archived = match self.archived_transaction(hash).await {
            HashLookup::Found((block, index)) => match self.converter.transaction(&block, index) {
                Some(tx) => HashLookup::Found(tx),
                None => HashLookup::Failed(ArchiveError::unavailable(
                    base_exoware_archive::Family::TxHash,
                    "located transaction index is past the block",
                )),
            },
            HashLookup::Miss => HashLookup::Miss,
            HashLookup::Unresolved(err) => HashLookup::Unresolved(err),
            HashLookup::Failed(err) => HashLookup::Failed(err),
        };
        ArchiveRpcBoundary::by_hash(local, archived)
    }

    async fn transaction_by_block_hash_and_index(
        &self,
        hash: B256,
        index: Index,
    ) -> RpcResult<Option<Transaction>> {
        let local = EthApiServer::transaction_by_block_hash_and_index(&self.eth, hash, index).await;
        let local_number = self.provider.block_number(hash).ok().flatten();
        if !self.needs_archive(&local, |tx| tx.block_number, local_number) {
            return local;
        }
        let build = |block: &PreparedBlock| Ok(self.converter.transaction(block, index.0));
        match local_number {
            Some(n) => self.block_answer(n, build).await,
            None => self.block_answer_by_hash(hash, local, build).await,
        }
    }

    async fn transaction_by_block_number_and_index(
        &self,
        number: BlockNumberOrTag,
        index: Index,
    ) -> RpcResult<Option<Transaction>> {
        let local = EthApiServer::transaction_by_block_number_and_index(&self.eth, number, index).await;
        let Some(n) = number.as_number() else { return local };
        if !self.needs_archive(&local, |tx| tx.block_number, Some(n)) {
            return local;
        }
        self.block_answer(n, |block| Ok(self.converter.transaction(block, index.0))).await
    }

    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<BaseTransactionReceipt>> {
        let local = EthApiServer::transaction_receipt(&self.eth, hash).await;
        if !self.needs_archive(&local, |r| r.inner.block_number, None) {
            return local;
        }
        let archived = match self.archived_transaction(hash).await {
            HashLookup::Found((block, index)) => match self.converter.receipt(&block, index) {
                Ok(Some(receipt)) => HashLookup::Found(receipt),
                Ok(None) => HashLookup::Failed(ArchiveError::unavailable(
                    base_exoware_archive::Family::TxHash,
                    "located transaction index is past the block",
                )),
                Err(err) => HashLookup::Failed(err),
            },
            HashLookup::Miss => HashLookup::Miss,
            HashLookup::Unresolved(err) => HashLookup::Unresolved(err),
            HashLookup::Failed(err) => HashLookup::Failed(err),
        };
        ArchiveRpcBoundary::by_hash(local, archived)
    }

    async fn block_receipts(&self, block: BlockId) -> RpcResult<Option<Vec<BaseTransactionReceipt>>> {
        let local = EthApiServer::block_receipts(&self.eth, block).await;
        let (number, hash) = match block {
            BlockId::Number(number) => match number.as_number() {
                Some(n) => (Some(n), None),
                None => return local,
            },
            BlockId::Hash(hash) => (self.provider.block_number(hash.block_hash).ok().flatten(), Some(hash.block_hash)),
        };
        let found = |receipts: &Vec<BaseTransactionReceipt>| receipts.first().and_then(|r| r.inner.block_number);
        if !self.needs_archive(&local, found, number) {
            return local;
        }
        let build = |block: &PreparedBlock| self.converter.receipts(block).map(Some);
        match (number, hash) {
            (Some(n), _) => self.block_answer(n, build).await,
            (None, Some(hash)) => self.block_answer_by_hash(hash, local, build).await,
            (None, None) => local,
        }
    }

    async fn logs(&self, filter: Filter) -> RpcResult<Vec<BaseLogResponse>> {
        match EngineEthFilter::logs(&self.filter, filter.clone(), self.limits).await {
            Err(err) if ArchiveRpcBoundary::is_pruned(&err) => self.archived_logs(filter).await,
            local => local,
        }
    }

    async fn balance(&self, address: Address, block: Option<BlockId>) -> RpcResult<U256> {
        let number = match self
            .local_state(block, StateDomain::Account, async || {
                EthApiServer::balance(&self.eth, address, block).await
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        self.archived_state(async |reader, request| {
            Ok(reader.account_at_in(request, address, number).await?.map(|a| a.balance).unwrap_or_default())
        })
        .await
    }

    async fn storage_at(
        &self,
        address: Address,
        slot: JsonStorageKey,
        block: Option<BlockId>,
    ) -> RpcResult<B256> {
        let number = match self
            .local_state(block, StateDomain::Storage, async || {
                EthApiServer::storage_at(&self.eth, address, slot, block).await
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        let key = slot.as_b256();
        self.archived_state(async |reader, request| {
            Ok(B256::from(reader.storage_at_in(request, address, key, number).await?))
        })
        .await
    }

    async fn transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        nonce_key: Option<U256>,
    ) -> RpcResult<U256> {
        if nonce_key.is_some_and(|key| !key.is_zero()) {
            return TxBaseEthApiOverrideServer::get_transaction_count(&self.base_tx, address, block, nonce_key)
                .await;
        }
        let number = match self
            .local_state(block, StateDomain::Account, async || {
                TxBaseEthApiOverrideServer::get_transaction_count(&self.base_tx, address, block, nonce_key)
                    .await
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        self.archived_state(async |reader, request| {
            Ok(U256::from(reader.account_at_in(request, address, number).await?.map(|a| a.nonce).unwrap_or_default()))
        })
        .await
    }

    async fn code(&self, address: Address, block: Option<BlockId>) -> RpcResult<Bytes> {
        let number = match self
            .local_state(block, StateDomain::Account, async || {
                EthApiServer::get_code(&self.eth, address, block).await
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        self.archived_state(async |reader, request| reader.code_at_in(request, address, number).await).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only log-limit violations are invalid params; every other archive failure is 4444 with
    /// the reason, never data.
    #[test]
    fn archive_errors_map_once_at_the_boundary() {
        let pruned = ArchiveRpcBoundary::error(ArchiveError::Pruned { block: 1, earliest: 8 });
        assert_eq!(pruned.code(), 4444, "pruned is 4444");
        assert_eq!(
            pruned.message(),
            "pruned history unavailable: block 1 is pruned from the archive (earliest archived block 8)",
            "message names the reason"
        );
        let outage = ArchiveRpcBoundary::error(ArchiveError::store("get", "connection refused"));
        assert_eq!(outage.code(), 4444, "outage is 4444");
        let fault = ArchiveRpcBoundary::error(ArchiveError::unavailable(
            base_exoware_archive::Family::Body,
            "root mismatch",
        ));
        assert_eq!(fault.code(), 4444, "integrity fault is 4444");
        let limit = ArchiveRpcBoundary::error(ArchiveError::LimitExceeded { reason: "x".into() });
        assert_eq!(limit.code(), INVALID_PARAMS_CODE, "limits are invalid params");
        assert!(ArchiveRpcBoundary::is_pruned(&pruned), "recognised as pruned");
    }

    /// Hash answers: found wins, a genuine miss or an unresolved outage keeps the node's null,
    /// and a failure after the hash resolved is 4444.
    #[test]
    fn hash_answers_pick_null_only_for_misses_and_unresolved_outages() {
        let local: RpcResult<Option<u64>> = Ok(None);
        assert_eq!(ArchiveRpcBoundary::by_hash(local.clone(), HashLookup::Found(7)).unwrap(), Some(7), "found");
        assert_eq!(ArchiveRpcBoundary::by_hash(local.clone(), HashLookup::Miss).unwrap(), None, "miss");
        assert_eq!(
            ArchiveRpcBoundary::by_hash(local.clone(), HashLookup::Unresolved(ArchiveError::store("range", "down")))
                .unwrap(),
            None,
            "unresolved outage keeps null"
        );
        let failed = ArchiveRpcBoundary::by_hash(local, HashLookup::Failed(ArchiveError::store("get", "down")));
        assert_eq!(failed.unwrap_err().code(), 4444, "resolved then failed is 4444");
    }

    /// reth's own limit errors are reproduced byte for byte.
    #[test]
    fn limit_errors_match_reth() {
        let err = ArchiveRpcBoundary::too_many_logs(2, 100, 101);
        assert_eq!(err.code(), INVALID_PARAMS_CODE, "invalid params");
        assert_eq!(err.message(), "query exceeds max results 2, retry with the range 100-101", "reth message");
        let err = ArchiveRpcBoundary::too_many_blocks(5);
        assert_eq!(err.message(), "query exceeds max block range 5", "reth message");
    }

    /// The combined log cap fails at the first crossing block like reth and never for a
    /// single-block query.
    #[test]
    fn combined_log_cap_matches_reth() {
        let log = |block: u64| BaseLogResponse {
            inner: alloy_rpc_types_eth::Log { block_number: Some(block), ..Default::default() },
            block_timestamp_ms: None,
        };
        let logs = vec![log(3), log(3), log(5), log(9)];
        assert!(ArchiveRpcBoundary::check_max_logs(None, &logs, 1, 10).is_ok(), "no cap");
        assert!(ArchiveRpcBoundary::check_max_logs(Some(4), &logs, 1, 10).is_ok(), "at the cap");
        let err = ArchiveRpcBoundary::check_max_logs(Some(2), &logs, 1, 10).unwrap_err();
        assert_eq!(err.message(), "query exceeds max results 2, retry with the range 1-4", "crossing at 5");
        let err = ArchiveRpcBoundary::check_max_logs(Some(1), &logs, 3, 10).unwrap_err();
        assert_eq!(err.message(), "query exceeds max results 1, retry with the range 3-3", "from crosses");
        assert!(ArchiveRpcBoundary::check_max_logs(Some(1), &logs[..2], 3, 3).is_ok(), "single block is exempt");
    }

    /// A node refusal of pruned state is recognised for the requested block only.
    #[test]
    fn state_pruned_refusal_is_recognised() {
        let refusal = ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(7)));
        assert!(ArchiveRpcBoundary::is_state_pruned(&refusal, 7), "refusal at the requested block");
        assert!(!ArchiveRpcBoundary::is_state_pruned(&refusal, 8), "another block");
    }
}
