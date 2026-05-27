//! Contains the forkchoice state for the L2.

use std::fmt::Display;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_provider::Network;
use alloy_rpc_types_eth::Block as RpcBlock;
use alloy_transport::TransportResult;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::Transaction;
use base_protocol::{BlockInfo, FromBlockError, L2BlockInfo};
use tracing::warn;

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
        let finalized = {
            let rpc_block =
                match get_block_compat(engine_client, BlockNumberOrTag::Finalized.into()).await {
                    Ok(Some(block)) => block,
                    Ok(None) => engine_client
                        .get_l2_block(cfg.genesis.l2.number.into())
                        .full()
                        .await?
                        .ok_or(SyncStartError::BlockNotFound(cfg.genesis.l2.number.into()))?,
                    Err(e) => return Err(e.into()),
                };

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
    let latest = get_block_compat(engine_client, BlockNumberOrTag::Latest.into())
        .await?
        .ok_or(SyncStartError::BlockNotFound(BlockNumberOrTag::Latest.into()))?;
    let latest_number = latest.header.number;

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
            Ok(_) => hi = mid,
            Err(FromBlockError::MissingL1InfoDeposit(_)) => lo = mid + 1,
            Err(err) => return Err(err.into()),
        }
    }

    warn!(
        target: "sync_start",
        block_number = lo,
        "found earliest unpruned block"
    );

    let block = engine_client
        .get_l2_block(lo.into())
        .full()
        .await?
        .ok_or(SyncStartError::BlockNotFound(lo.into()))?;
    let consensus_block = block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner());
    L2BlockInfo::from_block_and_genesis(&consensus_block, &cfg.genesis).map_err(Into::into)
}

/// Wrapper function around [`EngineClient::get_l2_block`] to handle compatibility issues with geth
/// and erigon. When serving a block-by-number request, these clients will return non-standard
/// errors for the safe and finalized heads when the chain has just started and nothing is marked as
/// safe or finalized yet.
async fn get_block_compat<EngineClient_: EngineClient>(
    engine_client: &EngineClient_,
    block_id: BlockId,
) -> TransportResult<Option<<Base as Network>::BlockResponse>> {
    match engine_client.get_l2_block(block_id).full().await {
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("block not found") || err_str.contains("Unknown block") {
                Ok(None)
            } else {
                Err(e)
            }
        }
        r => r,
    }
}
