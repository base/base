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
//! failed before a transaction hash the node cannot place resolved (so pending-transaction
//! polling does not turn into errors during an outage). A block hash the node does not know
//! is never pruned history (the node keeps every canonical header), so it keeps the node's
//! answer without a store call.

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
use base_exoware_archive::{ArchiveError, Family, LogFilter};
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
    AnswerNeeds, ArchiveConverter, ArchiveHandle, ArchiveProvider, HashLookup, HashMiss,
    LocalRetention, PreparedBlock, RetentionCache, StateDomain,
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
    async fn block_receipts(
        &self,
        block: BlockId,
    ) -> RpcResult<Option<Vec<BaseTransactionReceipt>>>;

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
        Err(Self::crossed_max_logs(max, from, logs[max].inner.block_number.unwrap_or(from)))
    }

    /// reth's max-logs error for a query from `from` whose first log past `max` is in block
    /// `crossing`: retry up to the block before it, or just `from` when `from` crosses.
    pub fn crossed_max_logs(max: usize, from: u64, crossing: u64) -> ErrorObjectOwned {
        let retry_to = if crossing == from { from } else { crossing - 1 };
        Self::too_many_logs(max, from, retry_to)
    }

    /// The block reth names in a "query exceeds max results …, retry with the range a-b" error.
    pub fn retry_to(err: &ErrorObjectOwned) -> Option<u64> {
        if err.code() != INVALID_PARAMS_CODE
            || !err.message().starts_with("query exceeds max results")
        {
            return None;
        }
        err.message().rsplit('-').next()?.parse().ok()
    }

    /// Whether the node answered with the EIP-4444 "pruned history" error.
    pub fn is_pruned(err: &ErrorObjectOwned) -> bool {
        err.code() == EthRpcErrorCode::PrunedHistory.code()
    }

    /// Whether a local read of block `number` that failed with `err` raced the node's pruner:
    /// a server-side failure (not 4444, not invalid params) after which a fresh retention read
    /// (bypassing the cache) puts `number`'s `needs` data below the node's boundaries. reth's
    /// static-file pruner can delete a file under an in-flight read, which surfaces as an
    /// internal I/O error. Any other failure keeps the node's error unchanged.
    pub fn raced_prune(
        err: &ErrorObjectOwned,
        number: u64,
        needs: AnswerNeeds,
        retention: &RetentionCache,
        read: impl FnOnce() -> LocalRetention,
    ) -> bool {
        !Self::is_pruned(err)
            && err.code() != INVALID_PARAMS_CODE
            && retention.refresh(read).incomplete(number, needs)
    }

    /// Whether the node refused a state read at `number` because its historical state
    /// provider found that history pruned (reth renders `StateAtBlockPruned` as an internal
    /// error).
    pub fn is_state_pruned(err: &ErrorObjectOwned, number: u64) -> bool {
        let expected =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(number)));
        (err.code() == expected.code() && err.message() == expected.message())
            || Self::is_insufficient_changesets(err)
    }

    /// Whether the node refused a state read with reth's `InsufficientChangesets` as an
    /// internal error. reth maps it to 4444 only when the request is below a non-degenerate
    /// available range; the changeset paths that report an empty range (`0..=0`) surface it as
    /// internal, and the requested block is then below what the node can revert to.
    pub fn is_insufficient_changesets(err: &ErrorObjectOwned) -> bool {
        err.code() == jsonrpsee::types::error::INTERNAL_ERROR_CODE
            && err.message().starts_with("insufficient changesets to revert to block #")
    }

    /// Picks the answer of a hash-addressed request after the node missed: the archive's
    /// answer, the node's answer for a genuine miss or an unresolved outage, else 4444.
    pub fn by_hash<T>(
        local: RpcResult<Option<T>>,
        archived: HashLookup<T>,
    ) -> RpcResult<Option<T>> {
        match archived {
            HashLookup::Found(found) => Ok(Some(found)),
            HashLookup::Miss | HashLookup::Unresolved(_) => local,
            HashLookup::Failed(err) => Err(Self::error(err)),
        }
    }
}

/// Which block a request names, as the node resolved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalIdentity {
    /// Block number.
    pub number: u64,
    /// Block hash the request (or the node's index) named; the archived block must match it.
    pub hash: Option<B256>,
}

/// How a block-level request addresses its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressed {
    /// By number.
    Number(u64),
    /// By hash.
    Hash(B256),
}

/// The node's pending transactions, consulted before a `null` transaction-hash answer may go
/// to the archive.
pub trait PendingPool: Send + Sync + std::fmt::Debug + 'static {
    /// Whether transaction `hash` is in the local pool.
    fn contains(&self, hash: &B256) -> bool;
}

impl<T: reth_transaction_pool::TransactionPool + std::fmt::Debug + 'static> PendingPool for T {
    fn contains(&self, hash: &B256) -> bool {
        reth_transaction_pool::TransactionPool::contains(self, hash)
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
    /// The node's transaction pool.
    pub pool: Arc<dyn PendingPool>,
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

    /// The node's retention read now, bypassing the cache.
    pub fn fresh_retention(&self) -> LocalRetention {
        self.retention.refresh(|| LocalRetention::read(&self.provider))
    }

    /// [`ArchiveRpcBoundary::raced_prune`] against this node's provider.
    pub fn raced_prune(&self, err: &ErrorObjectOwned, number: u64, needs: AnswerNeeds) -> bool {
        let raced = ArchiveRpcBoundary::raced_prune(err, number, needs, &self.retention, || {
            LocalRetention::read(&self.provider)
        });
        if raced {
            debug!(
                number,
                "local read failed while the node pruned the block; serving from the archive"
            );
        }
        raced
    }

    /// Whether the node's `needs` data for block `number` is pruned now. The cached boundary
    /// answers "pruned" directly (pruning only moves forward); otherwise the boundary is read
    /// again, so a call made after a local read validates that read against fresh boundaries.
    /// Called only for answers that are visibly incomplete or missing, never for complete ones.
    pub fn pruned_now(&self, number: u64, needs: AnswerNeeds) -> bool {
        self.retention.replace_local(true, number, needs, || LocalRetention::read(&self.provider))
    }

    /// Timestamp of local block `number`, if the node still has its header.
    pub fn block_timestamp(&self, number: u64) -> Option<u64> {
        self.provider.header_by_number(number).ok().flatten().map(|header| header.timestamp)
    }

    /// Whether a mined deposit receipt visibly lacks a deposit field its fork requires. The
    /// block's header is read only for a deposit receipt with a field absent.
    pub fn receipt_incomplete(&self, receipt: &BaseTransactionReceipt) -> bool {
        let Some((nonce, version)) = ArchiveConverter::receipt_deposit_fields(receipt) else {
            return false;
        };
        if nonce.is_some() && version.is_some() {
            return false;
        }
        receipt.inner.block_number.and_then(|number| self.block_timestamp(number)).is_some_and(
            |timestamp| self.converter.deposit_fields_missing(timestamp, nonce, version),
        )
    }

    /// Whether a local miss for block `number` may be pruned history: the block exists (at or
    /// below the head) and its `needs` data is pruned now.
    pub fn miss_may_be_pruned(&self, number: u64, needs: AnswerNeeds) -> bool {
        self.provider.best_block_number().is_ok_and(|best| number <= best)
            && self.pruned_now(number, needs)
    }

    /// The node's identity for `addressed`: the number as given, or the number the node's
    /// header index maps the hash to.
    pub fn identity(&self, addressed: Addressed) -> Option<LocalIdentity> {
        match addressed {
            Addressed::Number(number) => Some(LocalIdentity { number, hash: None }),
            Addressed::Hash(hash) => self
                .provider
                .block_number(hash)
                .ok()
                .flatten()
                .map(|number| LocalIdentity { number, hash: Some(hash) }),
        }
    }

    /// Runs `read` inside one archive request within the request budget.
    pub async fn archive_request<T>(
        &self,
        read: impl AsyncFnOnce(
            &crate::NodeArchiveReader<P>,
            &base_exoware_archive::ArchiveRequest<base_exoware_archive::ExowareSession>,
        ) -> Result<T, ArchiveError>,
    ) -> Result<T, ArchiveError> {
        self.archive.request(read).await
    }

    /// Requires the archived block to carry the hash the request named.
    pub fn require_hash(block: &PreparedBlock, expected: Option<B256>) -> Result<(), ArchiveError> {
        match expected {
            Some(expected) if block.block.hash() != expected => Err(ArchiveError::unavailable(
                Family::BlockHash,
                format!(
                    "archived block {} has hash {}, the request named {expected}",
                    block.block.header().number,
                    block.block.hash()
                ),
            )),
            _ => Ok(()),
        }
    }

    /// Reads block `id.number` from the archive and checks it is the block `id` names.
    pub async fn archived_at(&self, id: LocalIdentity) -> Result<PreparedBlock, ArchiveError> {
        let block = self
            .archive_request(async |reader, request| reader.block_in(request, id.number).await)
            .await
            .map(|block| self.converter.prepare(block))?;
        Self::require_hash(&block, id.hash)?;
        Ok(block)
    }

    /// Resolves a transaction hash the node cannot place in the archive and reads its block.
    /// Resolution is marked as soon as a valid locator decodes.
    pub async fn archived_transaction(&self, hash: B256) -> HashLookup<(PreparedBlock, usize)> {
        let mut resolved = false;
        let outcome = self
            .archive_request(async |reader, request| {
                let Some((number, index)) = reader.locate_transaction_in(request, hash).await?
                else {
                    return Ok(None);
                };
                resolved = true;
                reader.transaction_at_in(request, hash, number, index).await?;
                Ok(Some((reader.block_in(request, number).await?, index as usize)))
            })
            .await
            .map(|found| found.map(|(block, index)| (self.converter.prepare(block), index)));
        ArchiveHandle::<P>::classify(outcome, resolved)
    }

    /// Index of transaction `hash` in an archived block.
    pub fn transaction_index(block: &PreparedBlock, hash: B256) -> Result<usize, ArchiveError> {
        block.block.body().transactions.iter().position(|tx| tx.tx_hash() == hash).ok_or_else(
            || {
                ArchiveError::unavailable(
                    Family::TxHash,
                    format!(
                        "archived block {} does not hold transaction {hash}",
                        block.block.header().number
                    ),
                )
            },
        )
    }

    /// A block-level answer from the archive for a block the node identified. `required`
    /// means the node answered with data the archive must reproduce, so a missing item is a
    /// fault, not `null`. Every failure is 4444.
    pub async fn answer_at<T>(
        &self,
        id: LocalIdentity,
        required: bool,
        build: impl FnOnce(&PreparedBlock) -> Result<Option<T>, ArchiveError>,
    ) -> RpcResult<Option<T>> {
        let answer = self.archived_at(id).await.and_then(|block| build(&block));
        match answer {
            Ok(None) if required => Err(ArchiveRpcBoundary::error(ArchiveError::unavailable(
                Family::Body,
                format!("archived block {} lacks an item the node served", id.number),
            ))),
            answer => answer.map_err(ArchiveRpcBoundary::error),
        }
    }

    /// Routes a block-level read. A local success is kept unless `incomplete` finds it visibly
    /// incomplete (a deposit lacking a field its fork requires; it returns the block number);
    /// only then are the boundaries checked, fresh, and a pruned answer is never reused: the
    /// archive answers or the request is 4444. Complete answers cost no retention read. A local
    /// miss, a pruned refusal, or a failure that raced the pruner
    /// ([`ArchiveRpcBoundary::raced_prune`]) goes to the archive at
    /// the node's identity for the block. A hash the node does not know is never pruned history
    /// (the node keeps every canonical header, and the archive holds only finalized canonical
    /// blocks), so it keeps the node's answer without a store call.
    pub async fn routed<T>(
        &self,
        local: RpcResult<Option<T>>,
        needs: AnswerNeeds,
        addressed: Addressed,
        incomplete: impl FnOnce(&T) -> Option<u64>,
        build: impl FnOnce(&PreparedBlock) -> Result<Option<T>, ArchiveError>,
    ) -> RpcResult<Option<T>> {
        if let Ok(Some(value)) = &local {
            if needs == AnswerNeeds::Body {
                return local;
            }
            let Some(number) = incomplete(value) else { return local };
            if !self.pruned_now(number, needs) {
                return local;
            }
            debug!(
                number,
                "local answer lacks deposit fields over pruned receipts; serving from the archive"
            );
            let named = match addressed {
                Addressed::Hash(hash) => Some(hash),
                Addressed::Number(_) => None,
            };
            return self.answer_at(LocalIdentity { number, hash: named }, true, build).await;
        }
        // An unknown hash has no identity: it is not pruned history, so the node's answer stands.
        let Some(id) = self.identity(addressed) else { return local };
        let miss = match &local {
            Ok(None) => true,
            Err(err) if ArchiveRpcBoundary::is_pruned(err) => false,
            Err(err) if self.raced_prune(err, id.number, needs) => false,
            Err(_) | Ok(Some(_)) => return local,
        };
        if miss && !self.miss_may_be_pruned(id.number, needs) {
            return local;
        }
        self.answer_at(id, false, build).await
    }

    /// The archive's answer for transaction `hash` in the block the node placed it in. The
    /// node knows the transaction is mined, so a missing item is a fault (4444), not `null`.
    pub async fn answer_transaction_at<T>(
        &self,
        id: LocalIdentity,
        hash: B256,
        build: impl FnOnce(&PreparedBlock, usize) -> Result<Option<T>, ArchiveError>,
    ) -> RpcResult<Option<T>> {
        self.answer_at(id, true, |block| build(block, Self::transaction_index(block, hash)?)).await
    }

    /// Routes a transaction-hash read (`incomplete` gives the block number and hash of a mined
    /// local answer that visibly lacks a deposit field its fork requires). A mined transaction
    /// the node can place, through its answer or its transaction index, keeps that identity:
    /// an incomplete answer over pruned data, or a missing one, is replaced by the archive's
    /// copy of exactly that block, and any archive failure is 4444. A `null` for a pooled hash,
    /// or on a node whose transaction lookup is unpruned, stays local without a store call
    /// (pending-transaction polling); only an unplaced hash on a lookup-pruned node is looked
    /// up through the archive's locator.
    pub async fn routed_transaction<T>(
        &self,
        hash: B256,
        local: RpcResult<Option<T>>,
        incomplete: impl FnOnce(&T) -> Option<(u64, B256)>,
        build: impl FnOnce(&PreparedBlock, usize) -> Result<Option<T>, ArchiveError>,
    ) -> RpcResult<Option<T>> {
        let miss = match &local {
            Ok(Some(value)) => {
                let Some((number, block_hash)) = incomplete(value) else { return local };
                if !self.pruned_now(number, AnswerNeeds::Deposits) {
                    return local;
                }
                return self
                    .answer_transaction_at(
                        LocalIdentity { number, hash: Some(block_hash) },
                        hash,
                        build,
                    )
                    .await;
            }
            Ok(None) => true,
            Err(err) if ArchiveRpcBoundary::is_pruned(err) => false,
            Err(err) => {
                let meta = self.provider.transaction_by_hash_with_meta(hash).ok().flatten();
                return match meta {
                    Some((_, meta))
                        if self.raced_prune(err, meta.block_number, AnswerNeeds::Deposits) =>
                    {
                        let id = LocalIdentity {
                            number: meta.block_number,
                            hash: Some(meta.block_hash),
                        };
                        self.answer_transaction_at(id, hash, build).await
                    }
                    _ => local,
                };
            }
        };
        let meta = if miss && self.pool.contains(&hash) {
            None
        } else {
            self.provider.transaction_by_hash_with_meta(hash).ok().flatten().map(|(_, meta)| meta)
        };
        let route = if miss {
            let pooled = self.pool.contains(&hash);
            let indexed = || self.provider.transaction_id(hash).ok().flatten().is_some();
            let retention = self.local_retention();
            retention.hash_miss(
                pooled,
                meta.map(|m| m.block_number),
                !pooled && meta.is_none() && indexed(),
            )
        } else {
            meta.map_or(HashMiss::Archive, |m| HashMiss::Mined(m.block_number))
        };
        match (route, meta) {
            (HashMiss::Local, _) => return local,
            (HashMiss::Mined(number), Some(meta)) => {
                if miss && !self.pruned_now(number, AnswerNeeds::Deposits) {
                    return local;
                }
                let id = LocalIdentity { number, hash: Some(meta.block_hash) };
                return self.answer_transaction_at(id, hash, build).await;
            }
            _ => {}
        }
        let archived = match self.archived_transaction(hash).await {
            HashLookup::Found((block, index)) => match build(&block, index) {
                Ok(Some(answer)) => HashLookup::Found(answer),
                Ok(None) => HashLookup::Failed(ArchiveError::unavailable(
                    Family::TxHash,
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

    /// The block number a state read at `block` addresses, when it can be outside the node's
    /// retention. A hash must be canonical on the node (which also satisfies
    /// `requireCanonical`): only canonical history is archived, and the archived state at that
    /// number is then the state of exactly that hash. Anything else stays with the node.
    pub fn state_number(&self, block: Option<BlockId>) -> Option<u64> {
        if !LocalRetention::may_be_pruned(block) {
            return None;
        }
        match block? {
            BlockId::Hash(hash) => {
                let number = self.provider.block_number(hash.block_hash).ok().flatten()?;
                (self.provider.block_hash(number).ok().flatten() == Some(hash.block_hash))
                    .then_some(number)
            }
            BlockId::Number(tag) => self.provider.convert_block_number(tag).ok().flatten(),
        }
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
        let number = self.state_number(block);
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
        self.archive_request(read).await.map_err(ArchiveRpcBoundary::error)
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

    /// Archived logs of `filter` over `[from, to]`. With `cap = Some((max, query_from, held))`
    /// (reth's `max`, the combined query's start and the logs it already holds), the reader
    /// stops once the archived part crosses the remaining allowance, and the error is reth's
    /// max-logs error for the combined query, exactly as
    /// [`ArchiveRpcBoundary::check_max_logs`] would report it.
    pub async fn archived_log_range(
        &self,
        filter: &Filter,
        from: u64,
        to: u64,
        cap: Option<(usize, u64, usize)>,
    ) -> RpcResult<Vec<BaseLogResponse>> {
        let log_filter = Self::log_filter(filter, from, to);
        let allowance = cap.map(|(max, _, held)| max.saturating_sub(held));
        let logs = self
            .archive_request(async |reader, request| {
                reader.logs_in(request, &log_filter, allowance).await
            })
            .await
            .map_err(|err| match (err, cap) {
                (ArchiveError::TooManyLogs { crossing, .. }, Some((max, query_from, _))) => {
                    ArchiveRpcBoundary::crossed_max_logs(max, query_from, crossing)
                }
                (err, _) => ArchiveRpcBoundary::error(err),
            })?;
        Ok(logs.iter().map(ArchiveConverter::log).collect())
    }

    /// Logs of a `blockHash` filter for block `number`, which the node knows by `hash` and
    /// refused as pruned. The archived logs must all carry the hash; any failure is 4444.
    pub async fn archived_hash_logs(
        &self,
        filter: &Filter,
        hash: B256,
        number: u64,
    ) -> RpcResult<Vec<BaseLogResponse>> {
        let check = |logs: Vec<BaseLogResponse>| -> Result<Vec<BaseLogResponse>, ArchiveError> {
            let foreign = logs
                .iter()
                .find(|log| log.inner.block_hash != Some(hash))
                .map(|log| log.inner.block_hash);
            foreign.map_or(Ok(logs), |block| {
                Err(ArchiveError::unavailable(
                    Family::BlockHash,
                    format!("archived log of block {block:?} does not belong to {hash}"),
                ))
            })
        };
        let logs = self.archived_log_range(filter, number, number, None).await?;
        check(logs).map_err(ArchiveRpcBoundary::error)
    }

    /// Serves `eth_getLogs` after the node refused pruned receipts. Blocks below the node's
    /// first complete block (read fresh, and re-read whenever the node refuses again) come
    /// from the archive, the rest from the node's filter, with no gap. reth's limits apply to
    /// the combined result: the local suffix gets only the remaining log allowance, and a
    /// suffix that overflows it is turned into reth's error for the combined range.
    pub async fn archived_logs(&self, filter: Filter) -> RpcResult<Vec<BaseLogResponse>> {
        let best = self
            .provider
            .best_block_number()
            .map_err(|err| ErrorObjectOwned::from(EthApiError::from(err)))?;
        let FilterBlockOption::Range { from_block, to_block } = filter.block_option else {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                "invalid block range params",
                None::<()>,
            ));
        };
        let (from, to) = (self.resolve(from_block, best)?, self.resolve(to_block, best)?);
        if from > to {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                "invalid block range params",
                None::<()>,
            ));
        }
        if let Some(max) = self.limits.max_blocks_per_filter.filter(|max| to - from > *max) {
            return Err(ArchiveRpcBoundary::too_many_blocks(max));
        }
        let max_logs = self.limits.max_logs_per_response;
        let mut out = Vec::new();
        let mut archived_to = from.checked_sub(1);
        let mut first_local = self.fresh_retention().first_complete();
        for _ in 0..3 {
            let archive_from = archived_to.map_or(from, |n| n + 1);
            if archive_from < first_local && archive_from <= to {
                let archive_to = to.min(first_local - 1);
                // reth exempts single-block queries from the cap, so the reader gets none.
                let cap = max_logs.filter(|_| from != to).map(|max| (max, from, out.len()));
                out.extend(self.archived_log_range(&filter, archive_from, archive_to, cap).await?);
                ArchiveRpcBoundary::check_max_logs(max_logs, &out, from, to)?;
                archived_to = Some(archive_to);
            }
            if to < first_local {
                return Ok(out);
            }
            let local_from = from.max(first_local);
            let allowance = max_logs.map(|max| max.saturating_sub(out.len()));
            let local_filter = filter.clone().from_block(local_from).to_block(to);
            let limits =
                QueryLimits { max_blocks_per_filter: None, max_logs_per_response: allowance };
            match EngineEthFilter::logs(&self.filter, local_filter, limits).await {
                Ok(logs) => {
                    out.extend(logs);
                    ArchiveRpcBoundary::check_max_logs(max_logs, &out, from, to)?;
                    return Ok(out);
                }
                Err(err) if ArchiveRpcBoundary::is_pruned(&err) => {
                    let moved = self.fresh_retention().first_complete();
                    if moved <= first_local {
                        return Err(err);
                    }
                    debug!(
                        first_local,
                        moved, "node pruned further during the query; extending the archive part"
                    );
                    first_local = moved;
                }
                Err(err) => {
                    let (Some(max), Some(suffix_retry)) =
                        (max_logs, ArchiveRpcBoundary::retry_to(&err))
                    else {
                        return Err(err);
                    };
                    if local_from == from {
                        return Err(err);
                    }
                    // reth reports `b = c - 1` for a crossing block `c` above the suffix start,
                    // and `b = local_from` both when `local_from` itself crosses and when
                    // `local_from + 1` does; one single-block query tells them apart.
                    let crossing = if suffix_retry > local_from {
                        suffix_retry + 1
                    } else {
                        let first = filter.clone().from_block(local_from).to_block(local_from);
                        let logs =
                            EngineEthFilter::logs(&self.filter, first, QueryLimits::no_limits())
                                .await?;
                        if out.len() + logs.len() > max { local_from } else { local_from + 1 }
                    };
                    return Err(ArchiveRpcBoundary::too_many_logs(max, from, crossing - 1));
                }
            }
        }
        Err(ArchiveRpcBoundary::error(ArchiveError::unavailable(
            Family::Receipts,
            "the node kept pruning receipts during the query",
        )))
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
        let needs = if full { AnswerNeeds::Deposits } else { AnswerNeeds::Body };
        let incomplete =
            |b: &BaseBlockResponse| self.converter.block_incomplete(b).then_some(b.header.number);
        self.routed(local, needs, Addressed::Number(n), incomplete, |block| {
            Ok(Some(self.converter.block(block, full)))
        })
        .await
    }

    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<BaseBlockResponse>> {
        let local = EthApiServer::block_by_hash(&self.eth, hash, full).await;
        let needs = if full { AnswerNeeds::Deposits } else { AnswerNeeds::Body };
        let incomplete =
            |b: &BaseBlockResponse| self.converter.block_incomplete(b).then_some(b.header.number);
        self.routed(local, needs, Addressed::Hash(hash), incomplete, |block| {
            Ok(Some(self.converter.block(block, full)))
        })
        .await
    }

    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        let local = EthApiServer::block_transaction_count_by_number(&self.eth, number).await;
        let Some(n) = number.as_number() else { return local };
        self.routed(
            local,
            AnswerNeeds::Body,
            Addressed::Number(n),
            |_| None,
            |block| Ok(Some(U256::from(block.block.body().transactions.len()))),
        )
        .await
    }

    async fn block_transaction_count_by_hash(&self, hash: B256) -> RpcResult<Option<U256>> {
        let local = EthApiServer::block_transaction_count_by_hash(&self.eth, hash).await;
        self.routed(
            local,
            AnswerNeeds::Body,
            Addressed::Hash(hash),
            |_| None,
            |block| Ok(Some(U256::from(block.block.body().transactions.len()))),
        )
        .await
    }

    async fn transaction_by_hash(&self, hash: B256) -> RpcResult<Option<Transaction>> {
        let local = EthApiServer::transaction_by_hash(&self.eth, hash).await;
        self.routed_transaction(
            hash,
            local,
            |tx| {
                tx.block_number.zip(tx.block_hash).filter(|(number, _)| {
                    self.converter.transaction_incomplete(tx, || self.block_timestamp(*number))
                })
            },
            |block, index| Ok(self.converter.transaction(block, index)),
        )
        .await
    }

    async fn transaction_by_block_hash_and_index(
        &self,
        hash: B256,
        index: Index,
    ) -> RpcResult<Option<Transaction>> {
        let local = EthApiServer::transaction_by_block_hash_and_index(&self.eth, hash, index).await;
        let incomplete = |tx: &Transaction| {
            let number = tx.block_number?;
            self.converter
                .transaction_incomplete(tx, || self.block_timestamp(number))
                .then_some(number)
        };
        self.routed(local, AnswerNeeds::Deposits, Addressed::Hash(hash), incomplete, |block| {
            Ok(self.converter.transaction(block, index.0))
        })
        .await
    }

    async fn transaction_by_block_number_and_index(
        &self,
        number: BlockNumberOrTag,
        index: Index,
    ) -> RpcResult<Option<Transaction>> {
        let local =
            EthApiServer::transaction_by_block_number_and_index(&self.eth, number, index).await;
        let Some(n) = number.as_number() else { return local };
        let incomplete = |tx: &Transaction| {
            let number = tx.block_number?;
            self.converter
                .transaction_incomplete(tx, || self.block_timestamp(number))
                .then_some(number)
        };
        self.routed(local, AnswerNeeds::Deposits, Addressed::Number(n), incomplete, |block| {
            Ok(self.converter.transaction(block, index.0))
        })
        .await
    }

    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<BaseTransactionReceipt>> {
        let local = EthApiServer::transaction_receipt(&self.eth, hash).await;
        self.routed_transaction(
            hash,
            local,
            |r| r.inner.block_number.zip(r.inner.block_hash).filter(|_| self.receipt_incomplete(r)),
            |block, index| self.converter.receipt(block, index),
        )
        .await
    }

    async fn block_receipts(
        &self,
        block: BlockId,
    ) -> RpcResult<Option<Vec<BaseTransactionReceipt>>> {
        let local = EthApiServer::block_receipts(&self.eth, block).await;
        let addressed = match block {
            BlockId::Number(number) => match number.as_number() {
                Some(n) => Addressed::Number(n),
                None => return local,
            },
            BlockId::Hash(hash) => Addressed::Hash(hash.block_hash),
        };
        let incomplete = |receipts: &Vec<BaseTransactionReceipt>| {
            receipts.iter().find(|r| self.receipt_incomplete(r)).and_then(|r| r.inner.block_number)
        };
        self.routed(local, AnswerNeeds::Deposits, addressed, incomplete, |block| {
            self.converter.receipts(block).map(Some)
        })
        .await
    }

    async fn logs(&self, filter: Filter) -> RpcResult<Vec<BaseLogResponse>> {
        let local = EngineEthFilter::logs(&self.filter, filter.clone(), self.limits).await;
        let Err(err) = &local else { return local };
        match filter.block_option {
            // A hash the node does not know is never pruned history; only a known block the
            // node refused as pruned goes to the archive.
            FilterBlockOption::AtBlockHash(hash) => {
                match self.provider.block_number(hash).ok().flatten() {
                    Some(number) if ArchiveRpcBoundary::is_pruned(err) => {
                        self.archived_hash_logs(&filter, hash, number).await
                    }
                    _ => local,
                }
            }
            FilterBlockOption::Range { .. } if ArchiveRpcBoundary::is_pruned(err) => {
                self.archived_logs(filter).await
            }
            FilterBlockOption::Range { .. } => local,
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
            Ok(reader
                .account_at_in(request, address, number)
                .await?
                .map(|a| a.balance)
                .unwrap_or_default())
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
            return TxBaseEthApiOverrideServer::get_transaction_count(
                &self.base_tx,
                address,
                block,
                nonce_key,
            )
            .await;
        }
        let number = match self
            .local_state(block, StateDomain::Account, async || {
                TxBaseEthApiOverrideServer::get_transaction_count(
                    &self.base_tx,
                    address,
                    block,
                    nonce_key,
                )
                .await
            })
            .await
        {
            Ok(local) => return local,
            Err(number) => number,
        };
        self.archived_state(async |reader, request| {
            Ok(U256::from(
                reader
                    .account_at_in(request, address, number)
                    .await?
                    .map(|a| a.nonce)
                    .unwrap_or_default(),
            ))
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
        self.archived_state(async |reader, request| {
            reader.code_at_in(request, address, number).await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use super::*;

    /// Test hook for the static-file race: the node's first local read fails with reth's
    /// internal I/O error while its pruner moves the boundaries to `pruned`; retention reads
    /// are counted.
    struct RacingNode {
        retention: Mutex<LocalRetention>,
        pruned: LocalRetention,
        failed: std::sync::atomic::AtomicBool,
        retention_reads: std::sync::atomic::AtomicUsize,
    }

    impl RacingNode {
        /// A node at `before` whose pruner reaches `pruned` during the first local read.
        fn new(before: LocalRetention, pruned: LocalRetention) -> Self {
            Self {
                retention: Mutex::new(before),
                pruned,
                failed: std::sync::atomic::AtomicBool::new(false),
                retention_reads: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// A local block read: fails once, as the pruner deletes the file under it.
        fn read_block(&self) -> RpcResult<Option<u64>> {
            if self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Ok(Some(1));
            }
            *self.retention.lock().unwrap() = self.pruned;
            Err(ErrorObjectOwned::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                "No such file or directory (os error 2)",
                None::<()>,
            ))
        }

        /// The node's retention now.
        fn read_retention(&self) -> LocalRetention {
            self.retention_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.retention.lock().unwrap()
        }

        /// Retention reads so far.
        fn reads(&self) -> usize {
            self.retention_reads.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A local read that fails with an internal error while the pruner advances is detected
    /// through a fresh retention read (the warm cache is bypassed) and goes to the archive;
    /// a failure for a block still above the boundary, a pruned refusal and invalid params
    /// keep the node's error unchanged, the latter two without a retention read.
    #[test]
    fn local_failures_that_raced_the_pruner_go_to_the_archive() {
        let before = LocalRetention::default();
        let pruned = LocalRetention { earliest_body: 5, receipts_pruned: Some(4), ..before };
        let cache = RetentionCache::new(Duration::from_secs(3600));
        let node = RacingNode::new(before, pruned);
        assert_eq!(cache.get(|| node.read_retention()), before, "the cache is warm and stale");
        assert_eq!(node.reads(), 1, "one warm-up read");

        let err = node.read_block().expect_err("the hook fails the first read");
        let original = err.clone();
        assert!(
            ArchiveRpcBoundary::raced_prune(&err, 1, AnswerNeeds::Body, &cache, || node
                .read_retention()),
            "block 1 is below the fresh earliest body: serve from the archive"
        );
        assert_eq!(node.reads(), 2, "exactly one fresh read, despite the warm cache");
        assert_eq!(
            cache.get(|| node.read_retention()),
            pruned,
            "the fresh read refreshed the cache"
        );
        assert_eq!(node.reads(), 2, "the refreshed cache answers without a read");
        assert!(
            ArchiveRpcBoundary::raced_prune(&err, 4, AnswerNeeds::Deposits, &cache, || node
                .read_retention()),
            "block 4's receipts are pruned now"
        );
        assert!(
            !ArchiveRpcBoundary::raced_prune(&err, 7, AnswerNeeds::Deposits, &cache, || node
                .read_retention()),
            "block 7 is above every boundary: the local error stands"
        );
        assert_eq!(node.reads(), 4, "each failed read checks fresh retention once");
        assert_eq!(
            (err.code(), err.message()),
            (original.code(), original.message()),
            "the node's error is unchanged"
        );
        assert_eq!(node.read_block(), Ok(Some(1)), "the hook fails only once");

        let refused = ArchiveRpcBoundary::error(ArchiveError::Pruned { block: 1, earliest: 8 });
        let invalid =
            ErrorObjectOwned::owned(INVALID_PARAMS_CODE, "invalid block range params", None::<()>);
        for other in [refused, invalid] {
            assert!(
                !ArchiveRpcBoundary::raced_prune(&other, 1, AnswerNeeds::Body, &cache, || node
                    .read_retention()),
                "{} is not a raced failure",
                other.message()
            );
        }
        assert_eq!(node.reads(), 4, "pruned refusals and invalid params read no retention");
    }

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
        assert_eq!(
            ArchiveRpcBoundary::by_hash(local.clone(), HashLookup::Found(7)).unwrap(),
            Some(7),
            "found"
        );
        assert_eq!(
            ArchiveRpcBoundary::by_hash(local.clone(), HashLookup::Miss).unwrap(),
            None,
            "miss"
        );
        assert_eq!(
            ArchiveRpcBoundary::by_hash(
                local.clone(),
                HashLookup::Unresolved(ArchiveError::store("range", "down"))
            )
            .unwrap(),
            None,
            "unresolved outage keeps null"
        );
        let failed = ArchiveRpcBoundary::by_hash(
            local,
            HashLookup::Failed(ArchiveError::store("get", "down")),
        );
        assert_eq!(failed.unwrap_err().code(), 4444, "resolved then failed is 4444");
    }

    /// reth's own limit errors are reproduced byte for byte.
    #[test]
    fn limit_errors_match_reth() {
        let err = ArchiveRpcBoundary::too_many_logs(2, 100, 101);
        assert_eq!(err.code(), INVALID_PARAMS_CODE, "invalid params");
        assert_eq!(
            err.message(),
            "query exceeds max results 2, retry with the range 100-101",
            "reth message"
        );
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
        assert_eq!(
            err.message(),
            "query exceeds max results 2, retry with the range 1-4",
            "crossing at 5"
        );
        let err = ArchiveRpcBoundary::check_max_logs(Some(1), &logs, 3, 10).unwrap_err();
        assert_eq!(
            err.message(),
            "query exceeds max results 1, retry with the range 3-3",
            "from crosses"
        );
        assert!(
            ArchiveRpcBoundary::check_max_logs(Some(1), &logs[..2], 3, 3).is_ok(),
            "single block is exempt"
        );
        for (max, from) in [(2, 1), (1, 3), (0, 1), (3, 1)] {
            let checked =
                ArchiveRpcBoundary::check_max_logs(Some(max), &logs, from, 10).unwrap_err();
            let crossing = logs[max].inner.block_number.unwrap();
            let early = ArchiveRpcBoundary::crossed_max_logs(max, from, crossing);
            assert_eq!(
                (early.code(), early.message()),
                (checked.code(), checked.message()),
                "the reader's early stop at block {crossing} renders the same error (max {max}, from {from})"
            );
        }
    }

    /// reth's `InsufficientChangesets` with an empty available range surfaces as an internal
    /// error and is recognised as pruned state; other internal errors are not.
    #[test]
    fn insufficient_changesets_is_a_state_fallback_trigger() {
        let degenerate =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::InsufficientChangesets {
                requested: 5,
                available: 0..=0,
            }));
        assert_eq!(
            degenerate.code(),
            jsonrpsee::types::error::INTERNAL_ERROR_CODE,
            "reth maps it to internal"
        );
        assert!(ArchiveRpcBoundary::is_state_pruned(&degenerate, 5), "recognised as pruned state");
        let below =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::InsufficientChangesets {
                requested: 5,
                available: 10..=20,
            }));
        assert!(ArchiveRpcBoundary::is_pruned(&below), "below the available range is 4444 already");
        let other = ErrorObjectOwned::from(EthApiError::from(ProviderError::BestBlockNotFound));
        assert!(
            !ArchiveRpcBoundary::is_state_pruned(&other, 5),
            "other internal errors stay local"
        );
    }

    /// The retry block is read back from reth's own limit error.
    #[test]
    fn retry_block_is_parsed_from_reth_errors() {
        assert_eq!(
            ArchiveRpcBoundary::retry_to(&ArchiveRpcBoundary::too_many_logs(3, 10, 42)),
            Some(42),
            "retry to 42"
        );
        assert_eq!(
            ArchiveRpcBoundary::retry_to(&ArchiveRpcBoundary::too_many_blocks(5)),
            None,
            "block cap is not a log cap"
        );
    }

    /// A node refusal of pruned state is recognised for the requested block only.
    #[test]
    fn state_pruned_refusal_is_recognised() {
        let refusal =
            ErrorObjectOwned::from(EthApiError::from(ProviderError::StateAtBlockPruned(7)));
        assert!(ArchiveRpcBoundary::is_state_pruned(&refusal, 7), "refusal at the requested block");
        assert!(!ArchiveRpcBoundary::is_state_pruned(&refusal, 8), "another block");
    }
}
