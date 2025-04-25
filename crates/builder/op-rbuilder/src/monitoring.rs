use alloy_consensus::{Transaction, TxReceipt};
use alloy_primitives::U256;
use futures_util::{Stream, StreamExt, TryStreamExt};
use reth::core::primitives::SignedTransaction;
use reth_chain_state::CanonStateNotification;
use reth_exex::{ExExContext, ExExEvent};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_optimism_primitives::{OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_primitives::{Block, RecoveredBlock};
use reth_provider::{Chain, ExecutionOutcome};
use tracing::{info, warn};

use crate::{metrics::OpRBuilderMetrics, tx_signer::Signer};

const OP_BUILDER_TX_PREFIX: &[u8] = b"Block Number:";

pub struct Monitoring {
    builder_signer: Option<Signer>,
    metrics: OpRBuilderMetrics,
}

impl Monitoring {
    pub fn new(builder_signer: Option<Signer>) -> Self {
        Self {
            builder_signer,
            metrics: Default::default(),
        }
    }

    #[allow(dead_code)]
    pub async fn run_with_exex<Node>(mut self, mut ctx: ExExContext<Node>) -> eyre::Result<()>
    where
        Node: FullNodeComponents<Types: NodeTypes<Primitives = OpPrimitives>>,
    {
        // Process all new chain state notifications
        while let Some(notification) = ctx.notifications.try_next().await? {
            if let Some(reverted_chain) = notification.reverted_chain() {
                self.revert(&reverted_chain).await?;
            }
            if let Some(committed_chain) = notification.committed_chain() {
                self.commit(&committed_chain).await?;
                ctx.events
                    .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
            }
        }

        Ok(())
    }

    pub async fn run_with_stream<St>(mut self, mut events: St) -> eyre::Result<()>
    where
        St: Stream<Item = CanonStateNotification<OpPrimitives>> + Unpin + 'static,
    {
        while let Some(event) = events.next().await {
            if let Some(reverted) = event.reverted() {
                self.revert(&reverted).await?;
            }

            let committed = event.committed();
            self.commit(&committed).await?;
        }

        Ok(())
    }

    /// Process a new chain commit.
    ///
    /// This function decodes the builder tx and then emits metrics
    async fn commit(&mut self, chain: &Chain<OpPrimitives>) -> eyre::Result<()> {
        info!("Processing new chain commit");
        let blocks = decode_chain_into_builder_txs(chain, self.builder_signer);

        for (block, has_builder_tx) in blocks {
            if has_builder_tx {
                self.metrics.inc_builder_landed_blocks();
                self.metrics.set_last_landed_block_height(block.number);
                info!(
                    block_number = block.number,
                    "Committed block built by builder"
                );
            } else {
                self.metrics.inc_builder_landed_blocks_missed();
            }
        }

        let num_reverted_tx = decode_chain_into_reverted_txs(chain);
        self.metrics.inc_num_reverted_tx(num_reverted_tx);

        let builder_balance =
            decode_state_into_builder_balance(chain.execution_outcome(), self.builder_signer)
                .and_then(|balance| {
                    balance
                        .to_string()
                        .parse::<f64>()
                        .map_err(|e| {
                            warn!("Failed to parse builder balance: {}", e);
                            e
                        })
                        .ok()
                });
        if let Some(balance) = builder_balance {
            self.metrics.set_builder_balance(balance);
        }

        Ok(())
    }

    /// Process a chain revert.
    ///
    /// This function decodes all transactions in the block, updates the metrics for builder built blocks
    async fn revert(&mut self, chain: &Chain<OpPrimitives>) -> eyre::Result<()> {
        info!("Processing new chain revert");
        let mut blocks = decode_chain_into_builder_txs(chain, self.builder_signer);
        // Reverse the order of txs to start reverting from the tip
        blocks.reverse();

        if let Some((block, _)) = blocks.last() {
            self.metrics.set_last_landed_block_height(block.number - 1);
        }

        for (block, has_builder_tx) in blocks {
            if has_builder_tx {
                self.metrics.dec_builder_landed_blocks();
                info!(
                    block_number = block.number,
                    "Reverted block built by builder"
                );
            }
        }

        Ok(())
    }
}

/// Decode chain of blocks and filter list to builder txs
fn decode_chain_into_builder_txs(
    chain: &Chain<OpPrimitives>,
    builder_signer: Option<Signer>,
) -> Vec<(&RecoveredBlock<Block<OpTransactionSigned>>, bool)> {
    chain
        // Get all blocks and receipts
        .blocks_and_receipts()
        // Get all receipts
        .map(|(block, receipts)| {
            let has_builder_tx =
                block
                    .body()
                    .transactions
                    .iter()
                    .zip(receipts.iter())
                    .any(move |(tx, receipt)| {
                        receipt.status()
                            && tx.input().starts_with(OP_BUILDER_TX_PREFIX)
                            && tx.recover_signer().is_ok_and(|signer| {
                                builder_signer.is_some_and(|bs| signer == bs.address)
                            })
                    });
            (block, has_builder_tx)
        })
        .collect()
}

/// Decode chain of blocks and check if any transactions has reverted
fn decode_chain_into_reverted_txs(chain: &Chain<OpPrimitives>) -> usize {
    chain
        // Get all blocks and receipts
        .blocks_and_receipts()
        // Get all receipts
        .map(|(block, receipts)| {
            block
                .body()
                .transactions
                .iter()
                .zip(receipts.iter())
                .filter(|(_, receipt)| !receipt.status())
                .count()
        })
        .sum()
}

/// Decode state and find the last builder balance
fn decode_state_into_builder_balance(
    execution_outcome: &ExecutionOutcome<OpReceipt>,
    builder_signer: Option<Signer>,
) -> Option<U256> {
    builder_signer.and_then(|signer| {
        execution_outcome
            .bundle
            .state
            .iter()
            .find(|(address, _)| *address == &signer.address)
            .and_then(|(_, account)| account.info.as_ref().map(|info| info.balance))
    })
}
