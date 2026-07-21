use base_shadow_canary_db::ShadowBlockRow;
use chrono::Utc;
use eyre::Result;
use futures::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::FullNodeComponents;
use reth_primitives_traits::{AlloyBlockHeader, NodePrimitives};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Shadow canary `ExEx` handler.
#[derive(Debug)]
pub struct ShadowCanaryExEx {
    tx: mpsc::Sender<ShadowBlockRow>,
}

impl ShadowCanaryExEx {
    /// Create a new shadow canary `ExEx` handler.
    pub const fn new(tx: mpsc::Sender<ShadowBlockRow>) -> Self {
        Self { tx }
    }

    /// Runs the shadow canary `ExEx` loop.
    pub async fn run<Node>(self, mut ctx: ExExContext<Node>) -> Result<()>
    where
        Node: FullNodeComponents,
    {
        while let Some(notification) = ctx.notifications.try_next().await? {
            let fully_processed = match &notification {
                ExExNotification::ChainCommitted { new } => {
                    debug!(
                        target: "base::shadow-canary",
                        block_number = new.tip().header().number(),
                        block_hash = ?new.tip().hash(),
                        "Committed chain notification received"
                    );
                    self.handle_chain_committed(new).await?
                }
                ExExNotification::ChainReorged { old, new } => {
                    info!(
                        target: "base::shadow-canary",
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
                        target: "base::shadow-canary",
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
                        target: "base::shadow-canary",
                        block_number = tip.number,
                        block_hash = ?tip.hash,
                        "Sending FinishedHeight event"
                    );
                    ctx.events.send(ExExEvent::FinishedHeight(tip))?;
                } else {
                    warn!(
                        target: "base::shadow-canary",
                        block_number = tip.number,
                        block_hash = ?tip.hash,
                        "Skipping FinishedHeight event after partial processing"
                    );
                }
            }
        }

        Ok(())
    }

    fn build_row_from_header(
        &self,
        header: &impl AlloyBlockHeader,
        block_hash: String,
        tx_count: usize,
        reorged_out: bool,
        canonical_hash: Option<String>,
    ) -> Result<ShadowBlockRow> {
        let number = i64::try_from(header.number())
            .map_err(|error| eyre::eyre!("block number overflow for shadow canary row: {error}"))?;
        let timestamp = i64::try_from(header.timestamp())
            .map_err(|error| eyre::eyre!("timestamp overflow for shadow canary row: {error}"))?;
        let tx_count = i32::try_from(tx_count).map_err(|error| {
            eyre::eyre!("transaction count overflow for shadow canary row: {error}")
        })?;
        let gas_used = i64::try_from(header.gas_used())
            .map_err(|error| eyre::eyre!("gas used overflow for shadow canary row: {error}"))?;
        let created_at = Utc::now();

        Ok(ShadowBlockRow {
            number,
            hash: block_hash,
            parent_hash: header.parent_hash().to_string(),
            timestamp,
            tx_count,
            gas_used,
            // Placeholder until builder metrics are wired for DA bytes.
            da_bytes: 0,
            state_root: header.state_root().to_string(),
            build_latency_ms: None,
            deadline_miss: false,
            fb_count: None,
            panicked: false,
            reorged_out,
            canonical_hash,
            // The writer injects the configured builder version before persistence.
            builder_version: String::new(),
            created_at,
        })
    }

    async fn handle_chain_committed<N>(&self, chain: &Chain<N>) -> Result<bool>
    where
        N: NodePrimitives,
    {
        for (block, receipts) in chain.blocks_and_receipts() {
            let header = block.header();
            let row = self.build_row_from_header(
                header,
                block.hash().to_string(),
                receipts.len(),
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
    {
        for (block, receipts) in old.blocks_and_receipts() {
            let header = block.header();
            let canonical_hash = new
                .blocks()
                .get(&header.number())
                .map(|canonical_block| canonical_block.hash().to_string());

            if canonical_hash.is_none() {
                warn!(
                    target: "base::shadow-canary",
                    block_number = header.number(),
                    old_block_hash = ?block.hash(),
                    new_tip_number = new.tip().header().number(),
                    new_tip_hash = ?new.tip().hash(),
                    "Missing canonical block for reorged shadow row"
                );
            }

            let row = self.build_row_from_header(
                header,
                block.hash().to_string(),
                receipts.len(),
                true,
                canonical_hash,
            )?;

            if !self.send_row(row).await? {
                return Ok(false);
            }
        }

        for (block, receipts) in new.blocks_and_receipts() {
            let header = block.header();
            let row = self.build_row_from_header(
                header,
                block.hash().to_string(),
                receipts.len(),
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
                    target: "base::shadow-canary",
                    error = ?error,
                    "Shadow canary writer channel closed"
                );
                Ok(false)
            }
        }
    }
}

/// Runs the shadow canary `ExEx` loop.
pub async fn run_exex<Node>(ctx: ExExContext<Node>, tx: mpsc::Sender<ShadowBlockRow>) -> Result<()>
where
    Node: FullNodeComponents,
{
    ShadowCanaryExEx::new(tx).run(ctx).await
}
