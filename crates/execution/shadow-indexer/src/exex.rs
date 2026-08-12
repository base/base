use alloy_consensus::{Transaction, TxReceipt, Typed2718, transaction::TxHashRef};
use base_shadow_indexer_db::{ShadowBlockPayload, ShadowBlockRow, ShadowTransaction};
use chrono::Utc;
use eyre::Result;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::FullNodeComponents;
use reth_primitives_traits::{
    AlloyBlockHeader, BlockBody, NodePrimitives, RecoveredBlock, SignedTransaction,
};
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
                    self.handle_chain_committed(new).await?
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
                        "ChainReverted notification ignored for S1 MVP"
                    );
                    true
                }
            };

            if let Some(committed_chain) = notification.committed_chain() {
                let tip = committed_chain.tip().num_hash();
                if fully_processed {
                    debug!(
                        target: "base::shadow-indexer",
                        block_number = tip.number,
                        block_hash = ?tip.hash,
                        "Sending FinishedHeight event"
                    );
                    ctx.events.send(ExExEvent::FinishedHeight(tip))?;
                } else {
                    warn!(
                        target: "base::shadow-indexer",
                        block_number = tip.number,
                        block_hash = ?tip.hash,
                        "Skipping FinishedHeight event after partial processing"
                    );
                }
            }
        }

        Ok(())
    }

    fn build_row(
        &self,
        header: &impl AlloyBlockHeader,
        block_hash: String,
        transactions: Vec<ShadowTransaction>,
        reorged_out: bool,
        canonical_hash: Option<String>,
    ) -> Result<ShadowBlockRow> {
        let number = i64::try_from(header.number()).map_err(|error| {
            eyre::eyre!("block number overflow for shadow indexer row: {error}")
        })?;
        let timestamp = i64::try_from(header.timestamp())
            .map_err(|error| eyre::eyre!("timestamp overflow for shadow indexer row: {error}"))?;
        let tx_count = i32::try_from(transactions.len()).map_err(|error| {
            eyre::eyre!("transaction count overflow for shadow indexer row: {error}")
        })?;
        let gas_used = i64::try_from(header.gas_used())
            .map_err(|error| eyre::eyre!("gas used overflow for shadow indexer row: {error}"))?;

        let payload = ShadowBlockPayload {
            parent_hash: header.parent_hash().to_string(),
            timestamp,
            tx_count,
            gas_used,
            state_root: header.state_root().to_string(),
            // The writer injects the configured builder version before persistence.
            builder_version: String::new(),
            transactions,
        };

        Ok(ShadowBlockRow {
            number,
            hash: block_hash,
            reorged_out,
            canonical_hash,
            created_at: Utc::now(),
            payload,
        })
    }

    fn build_transactions<N>(
        block: &RecoveredBlock<N::Block>,
        receipts: &[N::Receipt],
    ) -> Result<Vec<ShadowTransaction>>
    where
        N: NodePrimitives,
        N::SignedTx: SignedTransaction,
        N::Receipt: TxReceipt,
    {
        let header = block.header();
        let base_fee = header.base_fee_per_gas();
        let base_fee_per_gas = base_fee
            .map(i64::try_from)
            .transpose()
            .map_err(|error| eyre::eyre!("base fee overflow for shadow indexer tx row: {error}"))?;

        let transactions = block.body().transactions();
        let senders = block.senders();
        let mut rows = Vec::with_capacity(transactions.len());
        let mut previous_cumulative_gas = 0u64;

        for (index, (transaction, receipt)) in transactions.iter().zip(receipts.iter()).enumerate()
        {
            let tx_index = i32::try_from(index).map_err(|error| {
                eyre::eyre!("transaction index overflow for shadow indexer tx row: {error}")
            })?;
            let cumulative_gas = receipt.cumulative_gas_used();
            let gas_used = i64::try_from(cumulative_gas.saturating_sub(previous_cumulative_gas))
                .map_err(|error| {
                    eyre::eyre!("gas used overflow for shadow indexer tx row: {error}")
                })?;
            previous_cumulative_gas = cumulative_gas;

            let effective_gas_price = transaction.effective_gas_price(base_fee);
            let effective_priority_fee_per_gas = Self::effective_priority_fee_per_gas(
                base_fee,
                effective_gas_price,
                transaction.max_priority_fee_per_gas(),
            );

            rows.push(ShadowTransaction {
                tx_index,
                tx_hash: transaction.tx_hash().to_string(),
                sender: senders.get(index).map(|sender| sender.to_string()),
                tx_type: i16::from(transaction.ty()),
                effective_priority_fee_per_gas: effective_priority_fee_per_gas
                    .map(|fee| fee.to_string()),
                base_fee_per_gas,
                gas_used,
            });
        }

        Ok(rows)
    }

    fn effective_priority_fee_per_gas(
        base_fee_per_gas: Option<u64>,
        effective_gas_price: u128,
        max_priority_fee_per_gas: Option<u128>,
    ) -> Option<u128> {
        base_fee_per_gas
            .map(|base_fee| effective_gas_price.saturating_sub(u128::from(base_fee)))
            .or(max_priority_fee_per_gas)
    }

    async fn handle_chain_committed<N>(&self, chain: &Chain<N>) -> Result<bool>
    where
        N: NodePrimitives,
        N::SignedTx: SignedTransaction,
        N::Receipt: TxReceipt,
    {
        for (block, receipts) in chain.blocks_and_receipts() {
            let transactions = Self::build_transactions::<N>(block, receipts)?;
            let row = self.build_row(
                block.header(),
                block.hash().to_string(),
                transactions,
                false,
                None,
            )?;

            if !self.send_row(row).await? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn handle_chain_reorged<N>(&self, old: &Chain<N>, new: &Chain<N>) -> Result<bool>
    where
        N: NodePrimitives,
        N::SignedTx: SignedTransaction,
        N::Receipt: TxReceipt,
    {
        for (block, receipts) in old.blocks_and_receipts() {
            let header = block.header();
            let canonical_hash = new
                .blocks()
                .get(&header.number())
                .map(|canonical_block| canonical_block.hash().to_string());

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

            let transactions = Self::build_transactions::<N>(block, receipts)?;
            let row = self.build_row(
                header,
                block.hash().to_string(),
                transactions,
                true,
                canonical_hash,
            )?;

            if !self.send_row(row).await? {
                return Ok(false);
            }
        }

        for (block, receipts) in new.blocks_and_receipts() {
            let transactions = Self::build_transactions::<N>(block, receipts)?;
            let row = self.build_row(
                block.header(),
                block.hash().to_string(),
                transactions,
                false,
                None,
            )?;

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

/// Runs the shadow indexer `ExEx` loop.
pub async fn run_exex<Node>(ctx: ExExContext<Node>, tx: mpsc::Sender<ShadowBlockRow>) -> Result<()>
where
    Node: FullNodeComponents,
{
    ShadowIndexerExEx::new(tx).run(ctx).await
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use reth_ethereum_primitives::{Block, EthPrimitives, Receipt};
    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_primitives_traits::RecoveredBlock;
    use tokio::sync::mpsc;

    use super::*;

    const NEW_CHAIN_VARIANT: u8 = 0xff;

    fn block_hash(number: u64, variant: u8) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[0] = number as u8;
        bytes[1] = variant;
        B256::new(bytes)
    }

    fn mk_block(number: u64, variant: u8) -> RecoveredBlock<Block> {
        let mut block: RecoveredBlock<Block> = Default::default();
        block.set_block_number(number);
        block.set_hash(block_hash(number, variant));
        block.set_parent_hash(block_hash(number.saturating_sub(1), variant));
        block
    }

    fn mk_chain(from: u64, to: u64, variant: u8) -> Chain<EthPrimitives> {
        let mut blocks = Vec::new();
        let mut receipts: Vec<Vec<Receipt>> = Vec::new();
        for number in from..=to {
            blocks.push(mk_block(number, variant));
            receipts.push(vec![Receipt::default()]);
        }
        let execution_outcome: ExecutionOutcome<Receipt> = ExecutionOutcome {
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
            exex.handle_chain_committed(&mk_chain(1, 3, 0)).await.expect("handle committed");
        assert!(processed);

        let rows = drain(rx);
        assert_eq!(rows.iter().map(|row| row.number).collect::<Vec<_>>(), vec![1, 2, 3]);
        for row in &rows {
            assert!(!row.reorged_out, "committed rows must not be reorged out");
            assert_eq!(row.canonical_hash, None, "committed rows carry no canonical hash");
            assert_eq!(
                row.payload.tx_count as usize,
                row.payload.transactions.len(),
                "tx_count matches the embedded transactions array"
            );
            assert_eq!(row.hash, block_hash(row.number as u64, 0).to_string());
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
            assert_eq!(row.hash, block_hash(row.number as u64, 0).to_string());
            assert_eq!(
                row.canonical_hash,
                Some(block_hash(row.number as u64, NEW_CHAIN_VARIANT).to_string()),
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
        assert_eq!(present.canonical_hash, Some(block_hash(6, NEW_CHAIN_VARIANT).to_string()));
    }

    #[test]
    fn effective_priority_fee_uses_base_fee_when_present() {
        let fee = ShadowIndexerExEx::effective_priority_fee_per_gas(Some(7), 20, Some(100));
        assert_eq!(fee, Some(13), "tip = effective_gas_price - base_fee");
    }

    #[test]
    fn effective_priority_fee_saturates_below_base_fee() {
        let fee = ShadowIndexerExEx::effective_priority_fee_per_gas(Some(50), 20, Some(100));
        assert_eq!(fee, Some(0), "effective price below base fee saturates to zero");
    }

    #[test]
    fn effective_priority_fee_falls_back_to_max_priority_without_base_fee() {
        let fee = ShadowIndexerExEx::effective_priority_fee_per_gas(None, 20, Some(100));
        assert_eq!(fee, Some(100), "without base fee, fall back to max priority fee");
    }
}
