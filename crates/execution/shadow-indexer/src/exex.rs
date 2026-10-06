use alloy_eips::BlockNumHash;
use base_common_consensus::{BaseBlock, BasePrimitives, BaseReceipt};
use base_shadow_indexer_db::{
    ShadowBlockPayload, ShadowBlockRow, ShadowCanonicalRef, ShadowHash, ShadowWrite,
};
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

use crate::ShadowExExMetrics;

/// Shadow indexer `ExEx` handler.
#[derive(Debug)]
pub struct ShadowIndexerExEx {
    tx: mpsc::Sender<ShadowWrite>,
}

impl ShadowIndexerExEx {
    /// Create a new shadow indexer `ExEx` handler.
    pub const fn new(tx: mpsc::Sender<ShadowWrite>) -> Self {
        Self { tx }
    }

    /// Runs the shadow indexer `ExEx` loop.
    pub async fn run<Node>(self, mut ctx: ExExContext<Node>) -> Result<()>
    where
        Node: FullNodeComponents,
        Node::Types: NodeTypes<Primitives = BasePrimitives>,
    {
        let mut last_finished_height = None;

        while let Some(notification) = ctx.notifications.try_next().await? {
            let is_syncing = ctx.network().is_syncing();
            let kind = Self::notification_kind(&notification);
            let fully_processed = {
                let _timer =
                    base_metrics::timed!(ShadowExExMetrics::notification_duration_seconds(kind));
                match &notification {
                    ExExNotification::ChainCommitted { new } => {
                        debug!(
                            target: "base::shadow-indexer",
                            block_number = new.tip().header().number(),
                            block_hash = ?new.tip().hash(),
                            "Committed chain notification received"
                        );
                        self.resolve_canonical_heights(new).await?
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

            // `FinishedHeight` lets the manager prune the ExEx WAL, so acknowledging a height the
            // node may still reorg discards the notification that would have recorded the block it
            // discards. While syncing, commits are settled history. Live, any commit can still be
            // reorged, so only a reorg's own replacement chain is safe to expose.
            Self::emit_finished_height(
                &ctx.events,
                &notification,
                is_syncing,
                &mut last_finished_height,
            )?;
        }

        Ok(())
    }

    fn emit_finished_height(
        events: &mpsc::UnboundedSender<ExExEvent>,
        notification: &ExExNotification<BasePrimitives>,
        is_syncing: bool,
        last_finished_height: &mut Option<BlockNumHash>,
    ) -> Result<()> {
        if Self::should_emit_finished_height(notification, is_syncing) {
            *last_finished_height =
                notification.committed_chain().map(|chain| chain.tip().num_hash());
        }

        // Re-emitting the last safe height wakes the manager so it continues draining its
        // notification buffer without advancing the WAL watermark to a speculative block.
        if let Some(tip) = *last_finished_height {
            debug!(
                target: "base::shadow-indexer",
                block_number = tip.number,
                block_hash = ?tip.hash,
                "Sending FinishedHeight event"
            );
            events.send(ExExEvent::FinishedHeight(tip))?;
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

    const fn notification_kind(notification: &ExExNotification<BasePrimitives>) -> &'static str {
        match notification {
            ExExNotification::ChainCommitted { .. } => "committed",
            ExExNotification::ChainReorged { .. } => "reorged",
            ExExNotification::ChainReverted { .. } => "reverted",
        }
    }

    fn build_row(
        &self,
        block: &RecoveredBlock<BaseBlock>,
        receipts: &[BaseReceipt],
        canonical_hash: Option<String>,
    ) -> Result<ShadowBlockRow> {
        let _timer = base_metrics::timed!(ShadowExExMetrics::build_row_duration_seconds());

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
            hash: ShadowHash::encode(block.hash().as_slice()),
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
        let mut unresolved = 0usize;

        for (block, receipts) in old.blocks_and_receipts() {
            let canonical_hash = new
                .blocks()
                .get(&block.header().number())
                .map(|canonical_block| ShadowHash::encode(canonical_block.hash().as_slice()));

            if canonical_hash.is_none() {
                unresolved = unresolved.saturating_add(1);
            }

            let row = self.build_row(block, receipts, canonical_hash)?;

            if !self.send_write(ShadowWrite::Reorged(Box::new(row))).await? {
                return Ok(false);
            }
        }

        if unresolved > 0 {
            debug!(
                target: "base::shadow-indexer",
                unresolved,
                new_tip_number = new.tip().header().number(),
                "Reorged rows await a later canonical block"
            );
        }

        Ok(true)
    }

    async fn handle_chain_reverted(&self, old: &Chain<BasePrimitives>) -> Result<bool> {
        for (block, receipts) in old.blocks_and_receipts() {
            let row = self.build_row(block, receipts, None)?;

            if !self.send_write(ShadowWrite::Reorged(Box::new(row))).await? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn resolve_canonical_heights(&self, new: &Chain<BasePrimitives>) -> Result<bool> {
        for block in new.blocks().values() {
            let number = i64::try_from(block.header().number()).map_err(|error| {
                eyre::eyre!("block number overflow for shadow indexer canonical ref: {error}")
            })?;
            let canonical =
                ShadowCanonicalRef { number, hash: ShadowHash::encode(block.hash().as_slice()) };

            if !self.send_write(ShadowWrite::Canonical(canonical)).await? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn send_write(&self, write: ShadowWrite) -> Result<bool> {
        let _timer = base_metrics::timed!(ShadowExExMetrics::send_blocked_seconds());

        match self.tx.send(write).await {
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
    use std::{sync::Arc, time::Duration};

    use alloy_consensus::Receipt;
    use alloy_primitives::B256;
    use futures::TryStreamExt;
    use reth_chain_state::ForkChoiceStream;
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::EthPrimitives;
    use reth_evm_ethereum::EthEvmConfig;
    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_exex::{ExExHandle, ExExManager, ExExNotificationSource, Wal};
    use reth_provider::{providers::BlockchainProvider, test_utils::create_test_provider_factory};
    use tokio::{
        sync::{mpsc, watch},
        time::timeout,
    };

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

    fn drain(mut rx: mpsc::Receiver<ShadowWrite>) -> Vec<ShadowWrite> {
        let mut writes = Vec::new();
        while let Ok(write) = rx.try_recv() {
            writes.push(write);
        }
        writes
    }

    fn drain_rows(rx: mpsc::Receiver<ShadowWrite>) -> Vec<ShadowBlockRow> {
        drain(rx)
            .into_iter()
            .filter_map(|write| match write {
                ShadowWrite::Reorged(row) => Some(*row),
                ShadowWrite::Canonical(_) => None,
            })
            .collect()
    }

    fn drain_canonical(rx: mpsc::Receiver<ShadowWrite>) -> Vec<ShadowCanonicalRef> {
        drain(rx)
            .into_iter()
            .filter_map(|write| match write {
                ShadowWrite::Canonical(canonical) => Some(canonical),
                ShadowWrite::Reorged(_) => None,
            })
            .collect()
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

        let rows = drain_rows(rx);
        assert_eq!(rows.len(), old.blocks().len(), "only old-chain blocks are emitted");

        for row in &rows {
            assert_eq!(row.hash, ShadowHash::encode(block_hash(row.number as u64, 0).as_slice()));
            assert_eq!(
                row.canonical_hash,
                Some(ShadowHash::encode(
                    block_hash(row.number as u64, NEW_CHAIN_VARIANT).as_slice()
                )),
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

        let rows = drain_rows(rx);
        let missing = rows.iter().find(|row| row.number == 9).expect("old block 9 reorged out");
        assert_eq!(missing.canonical_hash, None, "no new block at height 9 => canonical hash None");

        let present = rows.iter().find(|row| row.number == 6).expect("old block 6 reorged out");
        assert_eq!(
            present.canonical_hash,
            Some(ShadowHash::encode(block_hash(6, NEW_CHAIN_VARIANT).as_slice()))
        );
    }

    #[tokio::test]
    async fn chain_reverted_emits_rows_without_canonical_hash() {
        let (tx, rx) = mpsc::channel(16);
        let exex = ShadowIndexerExEx::new(tx);

        let processed =
            exex.handle_chain_reverted(&mk_chain(4, 6, 0)).await.expect("handle reverted");
        assert!(processed);

        let rows = drain_rows(rx);
        assert_eq!(rows.iter().map(|row| row.number).collect::<Vec<_>>(), vec![4, 5, 6]);
        for row in &rows {
            assert_eq!(
                row.canonical_hash, None,
                "reverted rows have no replacement canonical hash"
            );
        }
    }

    #[tokio::test]
    async fn chain_committed_emits_canonical_refs_for_every_height() {
        let (tx, rx) = mpsc::channel(32);
        let exex = ShadowIndexerExEx::new(tx);

        let processed = exex
            .resolve_canonical_heights(&mk_chain(6, 9, NEW_CHAIN_VARIANT))
            .await
            .expect("handle committed");
        assert!(processed);

        let canonical = drain_canonical(rx);
        assert_eq!(
            canonical.iter().map(|entry| entry.number).collect::<Vec<_>>(),
            vec![6, 7, 8, 9],
            "every committed height can resolve a row discarded at that height"
        );
        for entry in &canonical {
            assert_eq!(
                entry.hash,
                ShadowHash::encode(block_hash(entry.number as u64, NEW_CHAIN_VARIANT).as_slice())
            );
        }
    }

    #[tokio::test]
    async fn commits_after_a_short_reorg_resolve_the_heights_it_left_unresolved() {
        let (tx, rx) = mpsc::channel(64);
        let exex = ShadowIndexerExEx::new(tx);

        // Production shape: five shadow blocks displaced by a single canonical block.
        exex.handle_chain_reorged(&mk_chain(6, 10, 0), &mk_chain(6, 6, NEW_CHAIN_VARIANT))
            .await
            .expect("handle reorged");
        for number in 7..=10 {
            exex.resolve_canonical_heights(&mk_chain(number, number, NEW_CHAIN_VARIANT))
                .await
                .expect("handle committed");
        }

        let writes = drain(rx);
        let unresolved: Vec<i64> = writes
            .iter()
            .filter_map(|write| match write {
                ShadowWrite::Reorged(row) if row.canonical_hash.is_none() => Some(row.number),
                _ => None,
            })
            .collect();
        assert_eq!(unresolved, vec![7, 8, 9, 10], "four heights are unresolved at reorg time");

        let resolved: Vec<i64> = writes
            .iter()
            .filter_map(|write| match write {
                ShadowWrite::Canonical(canonical) => Some(canonical.number),
                ShadowWrite::Reorged(_) => None,
            })
            .collect();
        assert_eq!(resolved, vec![7, 8, 9, 10], "later commits cover exactly those heights");
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

    #[test]
    fn repeats_last_safe_finished_height_for_speculative_commits() {
        let (events, mut emitted) = mpsc::unbounded_channel();
        let speculative = ExExNotification::ChainCommitted { new: Arc::new(mk_chain(2, 2, 0)) };
        let reorged = ExExNotification::ChainReorged {
            old: Arc::new(mk_chain(1, 1, 0)),
            new: Arc::new(mk_chain(1, 1, NEW_CHAIN_VARIANT)),
        };

        let mut last_finished_height = None;
        ShadowIndexerExEx::emit_finished_height(
            &events,
            &speculative,
            false,
            &mut last_finished_height,
        )
        .expect("handle speculative commit before reorg");
        assert!(emitted.is_empty(), "no speculative height is advertised before a reorg");

        ShadowIndexerExEx::emit_finished_height(
            &events,
            &reorged,
            false,
            &mut last_finished_height,
        )
        .expect("handle reorg");
        let safe_height = reorged.committed_chain().unwrap().tip().num_hash();
        assert_eq!(
            emitted.try_recv().expect("reorg height emitted"),
            ExExEvent::FinishedHeight(safe_height)
        );

        for number in 2..=4 {
            let speculative =
                ExExNotification::ChainCommitted { new: Arc::new(mk_chain(number, number, 0)) };
            ShadowIndexerExEx::emit_finished_height(
                &events,
                &speculative,
                false,
                &mut last_finished_height,
            )
            .expect("handle speculative commit after reorg");
            assert_eq!(
                emitted.try_recv().expect("safe height re-emitted"),
                ExExEvent::FinishedHeight(safe_height),
                "each speculative commit re-emits the canonical reorg height"
            );
        }
        assert!(emitted.is_empty());
    }

    #[tokio::test]
    async fn repeated_finished_height_drains_stalled_exex_manager_buffer() {
        const MANAGER_CAPACITY: usize = 4;

        let provider_factory = create_test_provider_factory();
        init_genesis(&provider_factory).expect("initialize genesis");
        let provider = BlockchainProvider::new(provider_factory).expect("create provider");
        let wal_dir = tempfile::tempdir().expect("create WAL directory");
        let wal = Wal::new(wal_dir.path()).expect("create WAL");
        let (exex_handle, events, mut notifications) = ExExHandle::new(
            "shadow-indexer".to_string(),
            Default::default(),
            provider.clone(),
            EthEvmConfig::mainnet(),
            wal.handle(),
        );
        let (finalized_headers, finalized_header_rx) = watch::channel(None);
        let manager = ExExManager::new(
            provider,
            vec![exex_handle],
            MANAGER_CAPACITY,
            wal,
            ForkChoiceStream::new(finalized_header_rx),
        );
        let manager_handle = manager.handle();

        let chain = Arc::new(Chain::<EthPrimitives>::new(
            vec![RecoveredBlock::default()],
            Default::default(),
            Default::default(),
        ));
        let manager_notification =
            ExExNotification::ChainReorged { old: Arc::clone(&chain), new: chain };
        for _ in 0..MANAGER_CAPACITY {
            manager_handle
                .send(ExExNotificationSource::Pipeline, manager_notification.clone())
                .expect("queue manager notification");
        }

        let manager_task = tokio::spawn(async move {
            let _finalized_headers = finalized_headers;
            manager.await
        });

        for _ in 0..2 {
            timeout(Duration::from_secs(1), notifications.try_next())
                .await
                .expect("manager delivers initial notification")
                .expect("read initial notification")
                .expect("notification stream remains open");
        }
        assert_eq!(manager_handle.capacity(), 2);
        assert!(
            timeout(Duration::from_millis(50), notifications.try_next()).await.is_err(),
            "manager stalls with two buffered notifications despite an empty ExEx channel"
        );

        let reorged = ExExNotification::ChainReorged {
            old: Arc::new(mk_chain(1, 1, 0)),
            new: Arc::new(mk_chain(1, 1, NEW_CHAIN_VARIANT)),
        };
        let speculative = ExExNotification::ChainCommitted { new: Arc::new(mk_chain(2, 2, 0)) };
        let mut last_finished_height = None;

        for notification in [&reorged, &speculative] {
            ShadowIndexerExEx::emit_finished_height(
                &events,
                notification,
                false,
                &mut last_finished_height,
            )
            .expect("emit canonical-safe finished height");
            timeout(Duration::from_secs(1), notifications.try_next())
                .await
                .expect("FinishedHeight wakes manager")
                .expect("read notification after wake")
                .expect("notification stream remains open");
        }

        assert_eq!(manager_handle.capacity(), MANAGER_CAPACITY);
        manager_task.abort();
        let _ = manager_task.await;
    }
}
