use base_common_consensus::{BaseBlock, BasePrimitives, BaseReceipt};
use base_shadow_indexer_db::{ShadowBlockPayload, ShadowBlockRow};
use chrono::Utc;
use eyre::Result;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_primitives_traits::{AlloyBlockHeader, RecoveredBlock};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

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
            let fully_processed = match &notification {
                ExExNotification::ChainCommitted { new } => {
                    debug!(
                        target: "base::shadow-indexer",
                        block_number = new.tip().header().number(),
                        block_hash = ?new.tip().hash(),
                        "Committed chain notification received"
                    );
                    self.emit_canonical_blocks(new).await?
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

            if let Some(committed_chain) = notification.committed_chain() {
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

    fn build_row(
        &self,
        block: &RecoveredBlock<BaseBlock>,
        receipts: &[BaseReceipt],
        reorged_out: bool,
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
            reorged_out,
            canonical_hash,
            created_at: now,
            updated_at: now,
            payload,
        })
    }

    async fn emit_canonical_blocks(&self, chain: &Chain<BasePrimitives>) -> Result<bool> {
        for (block, receipts) in chain.blocks_and_receipts() {
            let row = self.build_row(block, receipts, false, None)?;

            if !self.send_row(row).await? {
                return Ok(false);
            }
        }

        Ok(true)
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

            let row = self.build_row(block, receipts, true, canonical_hash)?;

            if !self.send_row(row).await? {
                return Ok(false);
            }
        }

        self.emit_canonical_blocks(new).await
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
            let row = self.build_row(block, receipts, true, None)?;

            if !self.send_row(row).await? {
                return Ok(false);
            }
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
    async fn chain_committed_emits_one_canonical_row_per_block() {
        let (tx, rx) = mpsc::channel(16);
        let exex = ShadowIndexerExEx::new(tx);

        let processed =
            exex.emit_canonical_blocks(&mk_chain(1, 3, 0)).await.expect("handle committed");
        assert!(processed);

        let rows = drain(rx);
        assert_eq!(rows.iter().map(|row| row.number).collect::<Vec<_>>(), vec![1, 2, 3]);
        for row in &rows {
            assert!(!row.reorged_out, "committed rows must not be reorged out");
            assert_eq!(row.canonical_hash, None, "committed rows carry no canonical hash");
            assert_eq!(
                row.payload.block.header().number(),
                row.number as u64,
                "payload block number matches the row"
            );
            assert_eq!(row.payload.receipts.len(), 1, "one receipt per block in the fixture");
            assert_eq!(row.hash.as_slice(), block_hash(row.number as u64, 0).as_slice());
        }
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
        let reorged: Vec<_> = rows.iter().filter(|row| row.reorged_out).collect();
        let canonical: Vec<_> = rows.iter().filter(|row| !row.reorged_out).collect();

        assert_eq!(reorged.len(), 3, "old blocks 6..=8 marked reorged out");
        assert_eq!(canonical.len(), 4, "new blocks 6..=9 recorded as canonical");

        for row in &reorged {
            assert_eq!(row.hash.as_slice(), block_hash(row.number as u64, 0).as_slice());
            assert_eq!(
                row.canonical_hash,
                Some(block_hash(row.number as u64, NEW_CHAIN_VARIANT).as_slice().to_vec()),
                "reorged-out row points at the new canonical hash at its height"
            );
        }
        for row in &canonical {
            assert_eq!(row.canonical_hash, None, "new canonical rows carry no canonical hash");
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
        let missing = rows
            .iter()
            .find(|row| row.number == 9 && row.reorged_out)
            .expect("old block 9 reorged out");
        assert_eq!(missing.canonical_hash, None, "no new block at height 9 => canonical hash None");

        let present = rows
            .iter()
            .find(|row| row.number == 6 && row.reorged_out)
            .expect("old block 6 reorged out");
        assert_eq!(
            present.canonical_hash,
            Some(block_hash(6, NEW_CHAIN_VARIANT).as_slice().to_vec())
        );
    }

    #[tokio::test]
    async fn chain_reverted_marks_all_rows_reorged_out_without_canonical_hash() {
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
}
