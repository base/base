//! Contains the forkchoice state for the L2.

use std::fmt::Display;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_provider::Network;
use alloy_rpc_types_eth::Block as RpcBlock;
use alloy_transport::TransportResult;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::Transaction;
use base_protocol::{BlockInfo, FromBlockError, L2BlockInfo};
use tracing::{error, warn};

use crate::{
    EngineClient, ForkchoiceCheckpointLabel, ForkchoiceCheckpointReader,
    NoopForkchoiceCheckpointReader, SyncStartError,
};

/// An unsafe, safe, and finalized [`L2BlockInfo`] returned by the [`crate::find_starting_forkchoice`]
/// function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L2ForkchoiceState {
    /// The unsafe L2 block.
    pub un_safe: L2BlockInfo,
    /// The safe L2 block.
    pub safe: L2BlockInfo,
    /// The finalized L2 block.
    pub finalized: L2BlockInfo,
}

impl Display for L2ForkchoiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FINALIZED: {} (#{}) | SAFE: {} (#{}) | UNSAFE: {} (#{})",
            self.finalized.block_info.hash,
            self.finalized.block_info.number,
            self.safe.block_info.hash,
            self.safe.block_info.number,
            self.un_safe.block_info.hash,
            self.un_safe.block_info.number,
        )
    }
}

impl L2ForkchoiceState {
    /// Fetches the current forkchoice state of the L2 execution layer.
    ///
    /// - The finalized block may not always be available. If it is not, we fall back to genesis.
    /// - The safe block may not always be available. If it is not, we fall back to the finalized
    ///   block.
    /// - The unsafe block is always assumed to be available.
    pub async fn current<EngineClient_: EngineClient>(
        cfg: &RollupConfig,
        engine_client: &EngineClient_,
    ) -> Result<Self, SyncStartError> {
        Self::current_with_checkpoint_reader(cfg, engine_client, &NoopForkchoiceCheckpointReader)
            .await
    }

    /// Like [`Self::current`], but falls back to `checkpoint_reader` for safe / finalized labels
    /// when reth has pruned the L1 info deposit transaction body.
    pub async fn current_with_checkpoint_reader<
        EngineClient_: EngineClient,
        CheckpointReader: ForkchoiceCheckpointReader + ?Sized,
    >(
        cfg: &RollupConfig,
        engine_client: &EngineClient_,
        checkpoint_reader: &CheckpointReader,
    ) -> Result<Self, SyncStartError> {
        let finalized = match get_block_compat(engine_client, BlockNumberOrTag::Finalized.into())
            .await
        {
            Ok(Some(rpc_block)) => {
                let rpc_block_number = rpc_block.header.number;
                match block_info_from_reth_or_checkpoint(
                    cfg,
                    ForkchoiceCheckpointLabel::Finalized,
                    rpc_block,
                    checkpoint_reader,
                )
                .await
                {
                    Ok(info) => info,
                    Err(SyncStartError::FromBlock(FromBlockError::MissingL1InfoDeposit(hash))) => {
                        warn!(
                            target: "sync_start",
                            block_hash = %hash,
                            block_number = rpc_block_number,
                            "finalized block body is pruned and no valid checkpoint exists, \
                             recovering to earliest unpruned block"
                        );
                        find_earliest_unpruned_block(cfg, engine_client, rpc_block_number).await?
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(None) => genesis_l2_block_info(cfg),
            Err(e) => return Err(e.into()),
        };
        let safe = match get_block_compat(engine_client, BlockNumberOrTag::Safe.into()).await {
            Ok(Some(block)) => {
                match block_info_from_reth_or_checkpoint(
                    cfg,
                    ForkchoiceCheckpointLabel::Safe,
                    block,
                    checkpoint_reader,
                )
                .await
                {
                    Ok(info) => info,
                    Err(SyncStartError::FromBlock(FromBlockError::MissingL1InfoDeposit(hash))) => {
                        warn!(
                            target: "sync_start",
                            block_hash = %hash,
                            "safe block body is pruned and no valid checkpoint exists, \
                             falling back to finalized"
                        );
                        finalized
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(None) => finalized,
            Err(e) => return Err(e.into()),
        };
        let un_safe = {
            let rpc_block = get_block_compat(engine_client, BlockNumberOrTag::Latest.into())
                .await?
                .ok_or(SyncStartError::BlockNotFound(BlockNumberOrTag::Latest.into()))?;
            L2BlockInfo::from_block_and_genesis(
                &rpc_block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
                &cfg.genesis,
            )?
        };

        Ok(Self { un_safe, safe, finalized })
    }
}

const fn genesis_l2_block_info(cfg: &RollupConfig) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo {
            hash: cfg.genesis.l2.hash,
            number: cfg.genesis.l2.number,
            // Base chains start at L2 block 0, whose parent hash is zero.
            parent_hash: B256::ZERO,
            timestamp: cfg.genesis.l2_time,
        },
        l1_origin: cfg.genesis.l1,
        seq_num: 0,
    }
}

async fn block_info_from_reth_or_checkpoint<
    CheckpointReader: ForkchoiceCheckpointReader + ?Sized,
>(
    cfg: &RollupConfig,
    label: ForkchoiceCheckpointLabel,
    rpc_block: RpcBlock<Transaction>,
    checkpoint_reader: &CheckpointReader,
) -> Result<L2BlockInfo, SyncStartError> {
    let block = rpc_block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner());
    match L2BlockInfo::from_block_and_genesis(&block, &cfg.genesis) {
        Ok(block_info) => Ok(block_info),
        Err(err @ FromBlockError::MissingL1InfoDeposit(_)) => {
            let header = BlockInfo::from(&block);
            let Some(checkpoint) = checkpoint_reader.checkpoint(label).await? else {
                return Err(err.into());
            };
            if checkpoint.block_info != header {
                warn!(
                    target: "sync_start",
                    label = label.as_str(),
                    reth_number = header.number,
                    reth_hash = %header.hash,
                    reth_parent_hash = %header.parent_hash,
                    reth_timestamp = header.timestamp,
                    checkpoint_number = checkpoint.block_info.number,
                    checkpoint_hash = %checkpoint.block_info.hash,
                    checkpoint_parent_hash = %checkpoint.block_info.parent_hash,
                    checkpoint_timestamp = checkpoint.block_info.timestamp,
                    "forkchoice checkpoint does not match reth labeled block header"
                );
                return Err(SyncStartError::CheckpointMismatch {
                    label,
                    reth_number: header.number,
                    reth_hash: header.hash,
                    checkpoint_number: checkpoint.block_info.number,
                    checkpoint_hash: checkpoint.block_info.hash,
                });
            }
            warn!(
                target: "sync_start",
                label = label.as_str(),
                number = checkpoint.block_info.number,
                hash = %checkpoint.block_info.hash,
                "using forkchoice checkpoint because reth block body is pruned"
            );
            Ok(checkpoint)
        }
        Err(err) => Err(err.into()),
    }
}

/// When the labeled safe or finalized block's body is pruned and no checkpoint is available,
/// finds the earliest L2 block whose body has not been pruned by performing a binary search
/// between the pruned block and the latest block. Used as a recovery fallback instead of
/// crashing or falling back to genesis (which would trigger a months-long re-derivation).
async fn find_earliest_unpruned_block<EngineClient_: EngineClient>(
    cfg: &RollupConfig,
    engine_client: &EngineClient_,
    pruned_block_number: u64,
) -> Result<L2BlockInfo, SyncStartError> {
    // Establish the upper-bound invariant for the binary search by fetching the `latest` block
    // and verifying that its body is available. The search relies on `hi` always pointing to a
    // block with an intact body; without this probe, two failure modes are possible:
    //
    //   1. `latest_number == pruned_block_number` (or only pruned blocks exist) — the
    //      `while lo < hi` loop never executes and the post-loop fetch would re-attempt the
    //      known-pruned block, re-raising the very `MissingL1InfoDeposit` we were recovering
    //      from.
    //   2. `latest` itself is pruned — every probed midpoint would shift `lo` upward, the
    //      search would converge on `latest_number`, and the post-loop hydrate would fail
    //      for the same reason.
    //
    // Probing once up front lets us return a precise error instead of crashing with a stale
    // `MissingL1InfoDeposit`, and also gives us a known-good `L2BlockInfo` we can return
    // immediately when the search range collapses.
    let latest = get_block_compat(engine_client, BlockNumberOrTag::Latest.into())
        .await?
        .ok_or(SyncStartError::BlockNotFound(BlockNumberOrTag::Latest.into()))?;
    let latest_number = latest.header.number;
    let latest_consensus =
        latest.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner());

    let mut last_known_unpruned =
        match L2BlockInfo::from_block_and_genesis(&latest_consensus, &cfg.genesis) {
            Ok(info) => info,
            Err(FromBlockError::MissingL1InfoDeposit(hash)) => {
                error!(
                    target: "sync_start",
                    latest_block_number = latest_number,
                    latest_block_hash = %hash,
                    "Latest L2 block body is pruned; cannot recover an unpruned upper bound"
                );
                return Err(SyncStartError::NoUnprunedBlockAvailable {
                    pruned_block_number,
                    latest_block_number: latest_number,
                });
            }
            Err(err) => return Err(err.into()),
        };

    if pruned_block_number >= latest_number {
        // Nothing to search above the pruned block. `latest` is by definition unpruned (just
        // validated above), so it is the earliest unpruned block we have.
        warn!(
            target: "sync_start",
            pruned_block_number,
            latest_block_number = latest_number,
            "Pruned block at or above latest; falling back to latest as earliest unpruned"
        );
        return Ok(last_known_unpruned);
    }

    // Binary search for the prune boundary between the known-pruned block and the latest block.
    // Invariant: blocks at `lo` have pruned bodies, blocks at `hi` have available bodies.
    let mut lo = pruned_block_number;
    let mut hi = latest_number;

    warn!(
        target: "sync_start",
        lo,
        hi,
        "binary searching for earliest unpruned block"
    );

    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let block = engine_client
            .get_l2_block(mid.into())
            .full()
            .await?
            .ok_or(SyncStartError::BlockNotFound(mid.into()))?;
        let consensus_block =
            block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner());

        match L2BlockInfo::from_block_and_genesis(&consensus_block, &cfg.genesis) {
            Ok(info) => {
                // Cache the last successfully hydrated block at the upper bound so the
                // post-loop return can avoid an extra round-trip — the value at `lo` after
                // the loop exits is necessarily this same block.
                last_known_unpruned = info;
                hi = mid;
            }
            Err(FromBlockError::MissingL1InfoDeposit(_)) => lo = mid + 1,
            Err(err) => return Err(err.into()),
        }
    }

    warn!(
        target: "sync_start",
        block_number = lo,
        "found earliest unpruned block"
    );

    Ok(last_known_unpruned)
}

/// Wrapper function around [`EngineClient::get_l2_block`] to handle compatibility issues with clients.
/// When serving a block-by-number request, these clients will return non-standard errors for the safe
/// and finalized heads when the chain has just started and nothing is marked as safe or finalized yet.
async fn get_block_compat<EngineClient_: EngineClient>(
    engine_client: &EngineClient_,
    block_id: BlockId,
) -> TransportResult<Option<<Base as Network>::BlockResponse>> {
    match engine_client.get_l2_block(block_id).full().await {
        Err(e) => {
            let err_str = e.to_string();
            // EIP-4444 error code for pruned state unavailable, or known string-based
            // "not found" responses from geth/erigon for safe/finalized when the chain
            // has just started and nothing is marked safe or finalized yet.
            if e.as_error_resp().is_some_and(|err| err.code == 4444)
                || err_str.contains("block not found")
                || err_str.contains("Unknown block")
            {
                Ok(None)
            } else {
                Err(e)
            }
        }
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumberOrTag;
    use alloy_json_rpc::ErrorPayload;

    use super::get_block_compat;
    use crate::test_utils::{MockL2BlockError, test_engine_client_builder};

    #[tokio::test]
    async fn get_block_compat_eip4444_error_code_returns_none() {
        let client = test_engine_client_builder()
            .with_l2_block_error(MockL2BlockError::ErrorResp(ErrorPayload {
                code: 4444,
                message: "history unavailable".into(),
                data: None,
            }))
            .build();

        let result = get_block_compat(&client, BlockNumberOrTag::Finalized.into()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_block_compat_block_not_found_string_returns_none() {
        let client = test_engine_client_builder()
            .with_l2_block_error(MockL2BlockError::Custom("block not found".into()))
            .build();

        let result = get_block_compat(&client, BlockNumberOrTag::Safe.into()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_block_compat_unknown_block_string_returns_none() {
        let client = test_engine_client_builder()
            .with_l2_block_error(MockL2BlockError::Custom("Unknown block".into()))
            .build();

        let result = get_block_compat(&client, BlockNumberOrTag::Safe.into()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_block_compat_unrecognized_error_propagates() {
        let client = test_engine_client_builder()
            .with_l2_block_error(MockL2BlockError::Custom("connection refused".into()))
            .build();

        let err = get_block_compat(&client, BlockNumberOrTag::Latest.into()).await.unwrap_err();
        assert!(err.to_string().contains("connection refused"));
    }
}
