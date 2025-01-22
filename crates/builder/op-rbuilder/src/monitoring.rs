use alloy_consensus::{Transaction, TxReceipt};
use futures_util::TryStreamExt;
use reth::core::primitives::SignedTransaction;
use reth_exex::{ExExContext, ExExEvent};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_optimism_primitives::{OpPrimitives, OpTransactionSigned};
use reth_primitives::{Block, SealedBlockWithSenders};
use reth_provider::Chain;
use tracing::info;

use crate::{metrics::OpRBuilderMetrics, tx_signer::Signer};

const OP_BUILDER_TX_PREFIX: &[u8] = b"Block Number:";

pub struct Monitoring<Node: FullNodeComponents> {
    ctx: ExExContext<Node>,
    builder_signer: Option<Signer>,
    metrics: OpRBuilderMetrics,
}

impl<Node> Monitoring<Node>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = OpPrimitives>>,
{
    pub fn new(ctx: ExExContext<Node>, builder_signer: Option<Signer>) -> Self {
        Self {
            ctx,
            builder_signer,
            metrics: Default::default(),
        }
    }

    pub async fn start(mut self) -> eyre::Result<()> {
        // Process all new chain state notifications
        while let Some(notification) = self.ctx.notifications.try_next().await? {
            if let Some(reverted_chain) = notification.reverted_chain() {
                self.revert(&reverted_chain).await?;
            }
            if let Some(committed_chain) = notification.committed_chain() {
                self.commit(&committed_chain).await?;
                self.ctx
                    .events
                    .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
            }
        }

        Ok(())
    }

    /// Process a new chain commit.
    ///
    /// This function decodes the builder tx and then emits metrics
    async fn commit(&mut self, chain: &Chain<OpPrimitives>) -> eyre::Result<()> {
        info!("Processing new chain commit");
        let txs = decode_chain_into_builder_txs(chain, self.builder_signer);

        for (block, _) in txs {
            self.metrics.inc_builder_built_blocks();
            self.metrics.set_last_built_block_height(block.number);
            info!(
                block_number = block.number,
                "Committed block built by builder"
            );
        }

        Ok(())
    }

    /// Process a chain revert.
    ///
    /// This function decodes all transactions in the block, updates the metrics for builder built blocks
    async fn revert(&mut self, chain: &Chain<OpPrimitives>) -> eyre::Result<()> {
        info!("Processing new chain revert");
        let mut txs = decode_chain_into_builder_txs(chain, self.builder_signer);
        // Reverse the order of txs to start reverting from the tip
        txs.reverse();

        if let Some((block, _)) = txs.last() {
            self.metrics.set_last_built_block_height(block.number - 1);
        }

        for (block, _) in txs {
            self.metrics.dec_builder_built_blocks();
            info!(
                block_number = block.number,
                "Reverted block built by builder"
            );
        }

        Ok(())
    }
}

/// Decode chain of blocks and filter list to builder txs
fn decode_chain_into_builder_txs(
    chain: &Chain<OpPrimitives>,
    builder_signer: Option<Signer>,
) -> Vec<(
    SealedBlockWithSenders<Block<OpTransactionSigned>>,
    OpTransactionSigned,
)> {
    chain
        // Get all blocks and receipts
        .blocks_and_receipts()
        // Get all receipts
        .flat_map(|(block, receipts)| {
            let block_clone = block.clone();
            block
                .body()
                .transactions
                .iter()
                .zip(receipts.iter().flatten())
                .filter_map(move |(tx, receipt)| {
                    let is_builder_tx = receipt.status()
                        && tx.input().starts_with(OP_BUILDER_TX_PREFIX)
                        && tx.recover_signer().is_some_and(|signer| {
                            builder_signer.is_some_and(|bs| signer == bs.address)
                        });

                    if is_builder_tx {
                        // Clone the entire block and the transaction
                        Some((block_clone.clone(), tx.clone()))
                    } else {
                        None
                    }
                })
        })
        .collect()
}
