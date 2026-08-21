use base_common_consensus::{BaseBlock, BasePrimitives, BaseReceipt};
use base_shadow_indexer_db::{ShadowBlockPayload, ShadowBlockRow};
use chrono::Utc;
use eyre::Result;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_network_api::NetworkInfo;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_primitives_traits::{AlloyBlockHeader, RecoveredBlock};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::ShadowIndexerMetrics;

/// Shadow indexer `ExEx` handler.
#[derive(Debug)]
pub struct ShadowIndexerExEx {
    tx: mpsc::Sender<ShadowBlockRow>,
}

impl ShadowIndexerExEx {
    /// Create a new shadow indexer `ExEx` handler.
    pub const fn new(tx: mpsc::Sender<ShadowBlockRow>) -> Self {
        Self { tx }
    }

    /// Runs the shadow indexer `ExEx` loop.
    pub async fn run<Node>(self, mut ctx: ExExContext<Node>) -> Result<()>
    where
        Node: FullNodeComponents,
        Node::Types: NodeTypes<Primitives = BasePrimitives>,
    {
        while let Some(notification) = ctx.notifications.try_next().await? {
            let is_syncing = ctx.network().is_syncing();
            let fully_processed = match &notification {
                ExExNotification::ChainCommitted { new } => {
                    debug!(
                        target: "base::shadow-indexer",
                        block_number = new.tip().header().number(),
                        block_hash = ?new.tip().hash(),
                        "Committed chain notification received; canonical rows are not persisted"
                    );
                    true
                }
                ExExNotification::ChainReorged { old, new } => {
                    info!(
                        target: "base::shadow-indexer",
                        old_block_number = old.tip().header().number(),
                        old_block_hash = ?old.tip().hash(),
                        new_block_number = new.tip().header().number(),
                        new_block_hash = ?new.tip().hash(),
                        "ChainReorged notification received"
                    );
                    self.handle_chain_reorged(old, new).await?
                }
                ExExNotification::ChainReverted { old } => {
                    info!(
                        target: "base::shadow-indexer",
                        old_block_number = old.tip().header().number(),
                        old_block_hash = ?old.tip().hash(),
                        "ChainReverted notification received"
                    );
                    self.handle_chain_reverted(old).await?
                }
            };

            // `fully_processed` is only false when the writer channel has closed, which is
            // terminal: every subsequent send would fail identically. Stop consuming instead
            // of looping, otherwise we never emit `FinishedHeight` again and the ExEx manager's
            // notification buffer grows unbounded (and `send_row` logs on every block).
            if !fully_processed {
                warn!(
                    target: "base::shadow-indexer",
                    "Shadow indexer writer channel closed; stopping ExEx"
                );
                break;
            }

            // Historical commits are canonical, but live commits are speculative shadow blocks.
            // In live mode only the replacement chain is safe to expose as the WAL watermark.
            if Self::should_emit_finished_height(&notification, is_syncing)
                && let Some(committed_chain) = notification.committed_chain()
            {
                let tip = committed_chain.tip().num_hash();
                debug!(
                    target: "base::shadow-indexer",
                    block_number = tip.number,
                    block_hash = ?tip.hash,
                    "Sending FinishedHeight event"
                );
                ctx.events.send(ExExEvent::FinishedHeight(tip))?;
            }
        }

        Ok(())
    }

    const fn should_emit_finished_height(
        notification: &ExExNotification<BasePrimitives>,
        is_syncing: bool,
    ) -> bool {
        match notification {
            ExExNotification::ChainCommitted { .. } => is_syncing,
            ExExNotification::ChainReorged { .. } => !is_syncing,
            ExExNotification::ChainReverted { .. } => false,
        }
    }

    fn build_row(
        &self,
        block: &RecoveredBlock<BaseBlock>,
        receipts: &[BaseReceipt],
        canonical_hash: Option<Vec<u8>>,
    ) -> Result<ShadowBlockRow> {
        let number = i64::try_from(block.header().number()).map_err(|error| {
            eyre::eyre!("block number overflow for shadow indexer row: {error}")
        })?;

        let payload = ShadowBlockPayload {
            // The writer injects the configured builder version before persistence.
            builder_version: String::new(),
            block: block.clone(),
            receipts: receipts.to_vec(),
        };

        let now = Utc::now();

        Ok(ShadowBlockRow {
            number,
            hash: block.hash().as_slice().to_vec(),
            // Always set: only reorged-out and reverted blocks are persisted now. The column
            // stays so the reader keeps filtering out canonical rows written by earlier builds.
            reorged_out: true,
            canonical_hash,
            created_at: now,
            updated_at: now,
            payload,
        })
    }

    async fn handle_chain_reorged(
        &self,
        old: &Chain<BasePrimitives>,
        new: &Chain<BasePrimitives>,
    ) -> Result<bool> {
        for (block, receipts) in old.blocks_and_receipts() {
            let header = block.header();
            let canonical_hash = new
                .blocks()
                .get(&header.number())
                .map(|canonical_block| canonical_block.hash().as_slice().to_vec());

            if canonical_hash.is_none() {
                warn!(
                    target: "base::shadow-indexer",
                    block_number = header.number(),
                    old_block_hash = ?block.hash(),
                    new_tip_number = new.tip().header().number(),
                    new_tip_hash = ?new.tip().hash(),
                    "Missing canonical block for reorged shadow row"
                );
            }

            let row = self.build_row(block, receipts, canonical_hash)?;

            if !self.send_row(row).await? {
                return Ok(false);
            }

            ShadowIndexerMetrics::reorged_blocks_total().increment(1);
        }

        Ok(true)
    }

    /// Marks every block in a reverted chain as reorged out.
    ///
    /// `ChainReverted` carries only the unwound blocks with no replacement chain, so there is no
    /// canonical hash at these heights (they may be re-synced later, arriving as `ChainCommitted`).
    /// reth emits it exclusively from the execution stage on a pipeline unwind (deep reorg
    /// requiring backfill, or a manual/consistency unwind); the live-sync engine path only ever
    /// produces `ChainCommitted`/`ChainReorged`.
    async fn handle_chain_reverted(&self, old: &Chain<BasePrimitives>) -> Result<bool> {
        for (block, receipts) in old.blocks_and_receipts() {
            let row = self.build_row(block, receipts, None)?;

            if !self.send_row(row).await? {
                return Ok(false);
            }

            ShadowIndexerMetrics::reorged_blocks_total().increment(1);
        }

        Ok(true)
    }

    async fn send_row(&self, row: ShadowBlockRow) -> Result<bool> {
        match self.tx.send(row).await {
            Ok(()) => Ok(true),
            Err(error) => {
                info!(
                    target: "base::shadow-indexer",
                    error = ?error,
                    "Shadow indexer writer channel closed"
                );
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Receipt;
    use alloy_primitives::B256;
    use reth_execution_types::{Chain, ExecutionOutcome};
    use tokio::sync::mpsc;

    use super::*;

    const NEW_CHAIN_VARIANT: u8 = 0xff;

    fn block_hash(number: u64, variant: u8) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[0] = number as u8;
        bytes[1] = variant;
        B256::new(bytes)
    }

    fn mk_block(number: u64, variant: u8) -> RecoveredBlock<BaseBlock> {
        let mut block: RecoveredBlock<BaseBlock> = Default::default();
        block.set_block_number(number);
        block.set_hash(block_hash(number, variant));
        block.set_parent_hash(block_hash(number.saturating_sub(1), variant));
        block
    }

    fn mk_chain(from: u64, to: u64, variant: u8) -> Chain<BasePrimitives> {
        let mut blocks = Vec::new();
        let mut receipts: Vec<Vec<BaseReceipt>> = Vec::new();
        for number in from..=to {
            blocks.push(mk_block(number, variant));
            receipts.push(vec![BaseReceipt::Eip1559(Receipt::default())]);
        }
        let execution_outcome: ExecutionOutcome<BaseReceipt> = ExecutionOutcome {
            bundle: Default::default(),
            receipts,
            requests: Vec::new(),
            first_block: from,
        };
        Chain::new(blocks, execution_outcome, Default::default())
    }

    fn drain(mut rx: mpsc::Receiver<ShadowBlockRow>) -> Vec<ShadowBlockRow> {
        let mut rows = Vec::new();
        while let Ok(row) = rx.try_recv() {
            rows.push(row);
        }
        rows
    }

    #[tokio::test]
    async fn chain_reorged_marks_old_rows_and_sets_canonical_hash() {
        let (tx, rx) = mpsc::channel(32);
        let exex = ShadowIndexerExEx::new(tx);

        // Old chain 6..=8 is reorged out; new canonical chain 6..=9 has distinct hashes.
        let old = mk_chain(6, 8, 0);
        let new = mk_chain(6, 9, NEW_CHAIN_VARIANT);
        let processed = exex.handle_chain_reorged(&old, &new).await.expect("handle reorged");
        assert!(processed);

        let rows = drain(rx);
        assert_eq!(rows.len(), old.blocks().len(), "only old-chain blocks are emitted");

        for row in &rows {
            assert!(row.reorged_out, "every persisted row is reorged out");
            assert_eq!(row.hash.as_slice(), block_hash(row.number as u64, 0).as_slice());
            assert_eq!(
                row.canonical_hash,
                Some(block_hash(row.number as u64, NEW_CHAIN_VARIANT).as_slice().to_vec()),
                "reorged-out row points at the new canonical hash at its height"
            );
        }
    }

    #[tokio::test]
    async fn chain_reorged_leaves_canonical_hash_none_when_height_missing() {
        let (tx, rx) = mpsc::channel(32);
        let exex = ShadowIndexerExEx::new(tx);

        // New chain is shorter than old, so old block 9 has no canonical counterpart.
        let old = mk_chain(6, 9, 0);
        let new = mk_chain(6, 8, NEW_CHAIN_VARIANT);
        exex.handle_chain_reorged(&old, &new).await.expect("handle reorged");

        let rows = drain(rx);
        let missing = rows.iter().find(|row| row.number == 9).expect("old block 9 reorged out");
        assert_eq!(missing.canonical_hash, None, "no new block at height 9 => canonical hash None");

        let present = rows.iter().find(|row| row.number == 6).expect("old block 6 reorged out");
        assert_eq!(
            present.canonical_hash,
            Some(block_hash(6, NEW_CHAIN_VARIANT).as_slice().to_vec())
        );
    }

    #[tokio::test]
    async fn chain_reverted_emits_rows_without_canonical_hash() {
        let (tx, rx) = mpsc::channel(16);
        let exex = ShadowIndexerExEx::new(tx);

        let processed =
            exex.handle_chain_reverted(&mk_chain(4, 6, 0)).await.expect("handle reverted");
        assert!(processed);

        let rows = drain(rx);
        assert_eq!(rows.iter().map(|row| row.number).collect::<Vec<_>>(), vec![4, 5, 6]);
        for row in &rows {
            assert!(row.reorged_out, "reverted rows are marked reorged out");
            assert_eq!(
                row.canonical_hash, None,
                "reverted rows have no replacement canonical hash"
            );
        }
    }

    #[test]
    fn should_emit_finished_height_tracks_sync_and_authoritative_reorgs() {
        let committed = ExExNotification::ChainCommitted { new: Arc::new(mk_chain(1, 1, 0)) };
        let reorged = ExExNotification::ChainReorged {
            old: Arc::new(mk_chain(1, 1, 0)),
            new: Arc::new(mk_chain(1, 1, NEW_CHAIN_VARIANT)),
        };
        let reverted = ExExNotification::ChainReverted { old: Arc::new(mk_chain(1, 1, 0)) };

        assert!(ShadowIndexerExEx::should_emit_finished_height(&committed, true));
        assert!(!ShadowIndexerExEx::should_emit_finished_height(&committed, false));
        assert!(!ShadowIndexerExEx::should_emit_finished_height(&reorged, true));
        assert!(ShadowIndexerExEx::should_emit_finished_height(&reorged, false));
        assert!(!ShadowIndexerExEx::should_emit_finished_height(&reverted, true));
        assert!(!ShadowIndexerExEx::should_emit_finished_height(&reverted, false));
    }
}
