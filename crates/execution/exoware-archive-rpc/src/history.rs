//! The archive as the node's [`PrunedHistory`]: state, blocks and transaction locations the
//! node pruned, for its own `eth_call`, `eth_callMany`, `trace_` and `debug_trace*` paths.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use base_common_consensus::BaseBlock;
use base_execution_rpc::{PrunedHistory, PrunedHistoryError, PrunedState, TransactionLocation};
use base_exoware_archive::{ArchiveError, Family};
use futures::future::BoxFuture;
use reth_primitives_traits::RecoveredBlock;
use tracing::debug;

use crate::{
    ArchiveConverter, ArchiveExecutor, ArchiveHandle, ArchiveProvider, ArchiveStateProvider,
    ExecutionFailure, HashLookup, HashMiss, LocalRetention, RetentionCache,
};

/// Serves the node's pruned history from the archive.
pub struct ExowareHistory<P> {
    /// Archive reader.
    pub archive: Arc<ArchiveHandle<P>>,
    /// Node provider.
    pub provider: P,
    /// Block recovery.
    pub converter: ArchiveConverter,
    /// The node's retention.
    pub retention: RetentionCache,
    /// Runtime the EVM's archive reads run on.
    pub executor: Arc<ArchiveExecutor>,
    /// Store budget of one whole execution.
    pub execution_budget: Duration,
}

impl<P> std::fmt::Debug for ExowareHistory<P> {
    /// The budget; the reader and provider are opaque.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExowareHistory")
            .field("execution_budget", &self.execution_budget)
            .finish_non_exhaustive()
    }
}

impl<P: ArchiveProvider> ExowareHistory<P> {
    /// Whether `pruned` holds for the node's retention: the cached boundary first (pruning
    /// only moves forward), then one read now.
    pub fn pruned_now(&self, pruned: impl Fn(&LocalRetention) -> bool) -> bool {
        let read = || LocalRetention::read(&self.provider);
        pruned(&self.retention.get(read)) || pruned(&self.retention.refresh(read))
    }

    /// Runs `fut` until `deadline`; past it the result is a store error naming the budget.
    pub async fn until<T>(
        &self,
        deadline: Instant,
        fut: impl Future<Output = Result<T, ArchiveError>>,
    ) -> Result<T, ArchiveError> {
        tokio::time::timeout_at(deadline.into(), fut).await.map_err(|_| {
            ArchiveError::store(
                "request",
                format!("execution budget of {:?} exceeded", self.execution_budget),
            )
        })?
    }

    /// Opens the archive request one execution at `number`/`hash` reads in, by `deadline`,
    /// after checking that the archived header of `number` is `hash`.
    pub async fn begin_state(
        &self,
        number: u64,
        hash: B256,
        deadline: Instant,
    ) -> Result<
        base_exoware_archive::ArchiveRequest<base_exoware_archive::ExowareSession>,
        ArchiveError,
    > {
        self.until(deadline, async {
            let reader = self.archive.reader().await?;
            let request = reader.begin().await?;
            let (_, found) = reader.header_in(&request, number).await?;
            if found != hash {
                return Err(ArchiveError::unavailable(
                    Family::BlockHash,
                    format!("archived block {number} has hash {found}, the node has {hash}"),
                ));
            }
            Ok(request)
        })
        .await
    }

    /// The verified archived block `number`, required to be `hash`.
    pub async fn archived_block(
        &self,
        number: u64,
        hash: B256,
    ) -> Result<RecoveredBlock<BaseBlock>, ArchiveError> {
        let block = self
            .archive
            .request(async |reader, request| reader.block_in(request, number).await)
            .await
            .map(|block| self.converter.prepare(block).block)?;
        if block.hash() != hash {
            return Err(ArchiveError::unavailable(
                Family::BlockHash,
                format!("archived block {number} has hash {}, the node has {hash}", block.hash()),
            ));
        }
        Ok(block)
    }

    /// Locates transaction `hash` in the archive and verifies the transaction at the located
    /// position hashes to it.
    pub async fn archived_location(&self, hash: B256) -> HashLookup<TransactionLocation> {
        let mut resolved = false;
        let outcome = self
            .archive
            .request(async |reader, request| {
                let Some((number, index)) = reader.locate_transaction_in(request, hash).await?
                else {
                    return Ok(None);
                };
                resolved = true;
                let tx = reader.transaction_at_in(request, hash, number, index).await?;
                Ok(Some((number, tx.block_hash, u64::from(index))))
            })
            .await;
        ArchiveHandle::<P>::classify(outcome, resolved)
    }
}

impl<P: ArchiveProvider> PrunedHistory<BaseBlock> for ExowareHistory<P> {
    /// Archived state for an execution the node refused as pruned. The whole execution,
    /// from `started` through its conclusion, has [`Self::execution_budget`].
    fn state_at(
        &self,
        number: u64,
        hash: B256,
        started: Instant,
    ) -> BoxFuture<'_, Result<PrunedState, PrunedHistoryError>> {
        Box::pin(async move {
            debug!(number, "node state is pruned; executing over archived state");
            let deadline = started + self.execution_budget;
            let request = Arc::new(
                self.begin_state(number, hash, deadline).await.map_err(PrunedHistoryError::new)?,
            );
            let failure = ExecutionFailure::default();
            let state = ArchiveStateProvider {
                number,
                archive: Arc::clone(&self.archive),
                request: Arc::clone(&request),
                provider: self.provider.clone(),
                executor: Arc::clone(&self.executor),
                deadline,
                failure: failure.clone(),
            };
            let archive = Arc::clone(&self.archive);
            let budget = self.execution_budget;
            let conclude = Box::pin(async move {
                let outcome = failure.get().map_or(Ok(()), Err);
                tokio::time::timeout_at(deadline.into(), async {
                    archive.reader().await?.conclude(&request, outcome).await
                })
                .await
                .map_err(|_| {
                    ArchiveError::store(
                        "request",
                        format!("execution budget of {budget:?} exceeded"),
                    )
                })
                .and_then(|concluded| concluded)
                .map_err(PrunedHistoryError::new)
            });
            Ok(PrunedState { state: Box::new(state), conclude, deadline })
        })
    }

    fn recovered_block(
        &self,
        number: u64,
        hash: B256,
    ) -> BoxFuture<'_, Result<Option<Arc<RecoveredBlock<BaseBlock>>>, PrunedHistoryError>> {
        Box::pin(async move {
            if !self.pruned_now(|retention| number < retention.earliest_body) {
                return Ok(None);
            }
            debug!(number, "node body is pruned; reading the archived block");
            self.archived_block(number, hash)
                .await
                .map(|block| Some(Arc::new(block)))
                .map_err(PrunedHistoryError::new)
        })
    }

    /// The archive's location of a hash the node cannot place. On a node whose lookup is
    /// unpruned an unindexed hash is never mined in the archive's range, so it stays a local
    /// miss without a store call; once the archive is consulted, only a verified miss is
    /// `None` and an outage is 4444.
    fn transaction_location(
        &self,
        hash: B256,
    ) -> BoxFuture<'_, Result<Option<TransactionLocation>, PrunedHistoryError>> {
        Box::pin(async move {
            let indexed = self.provider.transaction_id(hash).ok().flatten().is_some();
            let read = || LocalRetention::read(&self.provider);
            let local = |retention: LocalRetention| {
                retention.hash_miss(false, None, indexed) == HashMiss::Local
            };
            // Pruning only moves forward: refresh a cached "unpruned" before trusting it.
            if local(self.retention.get(read)) && local(self.retention.refresh(read)) {
                return Ok(None);
            }
            match self.archived_location(hash).await {
                HashLookup::Found(location) => Ok(Some(location)),
                HashLookup::Miss => Ok(None),
                HashLookup::Unresolved(err) | HashLookup::Failed(err) => {
                    Err(PrunedHistoryError::new(err))
                }
            }
        })
    }
}
