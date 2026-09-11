use std::{
    collections::VecDeque,
    fmt::Debug,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use alloy_eips::BlockNumHash;
use base_common_observability_tracing::tracing::debug;
use base_common_types_chain::BlockHeader;
use base_common_types_payload::ExExHead;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_provider::{
    BlockNumReader, BlockReader, Chain, HeaderProvider, StateProviderFactory,
};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::Receiver;

use crate::{BackfillJobFactory, ExExNotification, StreamBackfillJob, WalHandle};

/// A stream of [`ExExNotification`]s. The stream will emit notifications for all blocks. If the
/// stream is configured with a head via [`ExExNotifications::set_with_head`] or
/// [`ExExNotifications::with_head`], it will run backfill jobs to catch up to the node head.
#[derive(Debug)]
pub struct ExExNotifications<P> {
    inner: ExExNotificationsInner<P>,
}

#[derive(Debug)]
enum ExExNotificationsInner<P> {
    /// A stream of [`ExExNotification`]s. The stream will emit notifications for all blocks.
    WithoutHead(ExExNotificationsWithoutHead<P>),
    /// A stream of [`ExExNotification`]s. The stream will only emit notifications for blocks that
    /// are committed or reverted after the given head.
    WithHead(Box<ExExNotificationsWithHead<P>>),
    /// Internal state used when transitioning between [`ExExNotificationsInner::WithoutHead`] and
    /// [`ExExNotificationsInner::WithHead`].
    Invalid,
}

impl<P> ExExNotificationsInner<P> {
    /// Returns the provider of the underlying stream.
    fn provider(&self) -> &P {
        match self {
            Self::WithoutHead(n) => &n.provider,
            Self::WithHead(n) => &n.provider,
            Self::Invalid => unreachable!(),
        }
    }
}

impl<P> ExExNotifications<P> {
    /// Creates a new stream of [`ExExNotifications`] without a head.
    pub const fn new(
        node_head: BlockNumHash,
        provider: P,
        evm_config: BaseEvmConfig,
        notifications: Receiver<ExExNotification>,
        wal_handle: WalHandle,
    ) -> Self {
        Self {
            inner: ExExNotificationsInner::WithoutHead(ExExNotificationsWithoutHead::new(
                node_head,
                provider,
                evm_config,
                notifications,
                wal_handle,
            )),
        }
    }

    /// As [`set_with_head`](ExExNotifications::set_with_head), but backfills up to the
    /// node's current canonical head rather than the head captured at construction.
    pub fn catch_up_with_head(&mut self, exex_head: ExExHead) -> eyre::Result<()>
    where
        P: BlockNumReader,
    {
        // Resolve the current canonical head before tearing down the stream state, so a failed
        // lookup leaves the stream untouched.
        let local_head: BlockNumHash = self.inner.provider().chain_info()?.into();

        let current = std::mem::replace(&mut self.inner, ExExNotificationsInner::Invalid);
        let (provider, evm_config, notifications, wal_handle) = match current {
            ExExNotificationsInner::WithoutHead(n) => {
                (n.provider, n.evm_config, n.notifications, n.wal_handle)
            }
            ExExNotificationsInner::WithHead(n) => {
                (n.provider, n.evm_config, n.notifications, n.wal_handle)
            }
            ExExNotificationsInner::Invalid => unreachable!(),
        };
        let with_head = ExExNotificationsWithHead::new(
            local_head,
            provider,
            evm_config,
            notifications,
            wal_handle,
            exex_head,
        );
        self.inner = ExExNotificationsInner::WithHead(Box::new(with_head));
        Ok(())
    }
}

impl<P> ExExNotifications<P>
where
    P: BlockReader + HeaderProvider + StateProviderFactory + Clone + Unpin + 'static,
{
    /// Subscribe to notifications without a head.
    pub fn set_without_head(&mut self) {
        let current = std::mem::replace(&mut self.inner, ExExNotificationsInner::Invalid);
        self.inner = ExExNotificationsInner::WithoutHead(match current {
            ExExNotificationsInner::WithoutHead(notifications) => notifications,
            ExExNotificationsInner::WithHead(notifications) => ExExNotificationsWithoutHead::new(
                notifications.initial_local_head,
                notifications.provider,
                notifications.evm_config,
                notifications.notifications,
                notifications.wal_handle,
            ),
            ExExNotificationsInner::Invalid => unreachable!(),
        });
    }

    /// Subscribe to notifications after the provided head.
    pub fn set_with_head(&mut self, exex_head: ExExHead) {
        let current = std::mem::replace(&mut self.inner, ExExNotificationsInner::Invalid);
        self.inner = ExExNotificationsInner::WithHead(match current {
            ExExNotificationsInner::WithoutHead(notifications) => {
                Box::new(notifications.with_head(exex_head))
            }
            ExExNotificationsInner::WithHead(notifications) => {
                Box::new(ExExNotificationsWithHead::new(
                    notifications.initial_local_head,
                    notifications.provider,
                    notifications.evm_config,
                    notifications.notifications,
                    notifications.wal_handle,
                    exex_head,
                ))
            }
            ExExNotificationsInner::Invalid => unreachable!(),
        });
    }

    /// Return this stream configured without a head.
    pub fn without_head(mut self) -> Self {
        self.set_without_head();
        self
    }

    /// Return this stream configured with the provided head.
    pub fn with_head(mut self, exex_head: ExExHead) -> Self {
        self.set_with_head(exex_head);
        self
    }
}

impl<P> Stream for ExExNotifications<P>
where
    P: BlockReader + HeaderProvider + StateProviderFactory + Clone + Unpin + 'static,
{
    type Item = eyre::Result<ExExNotification>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match &mut self.get_mut().inner {
            ExExNotificationsInner::WithoutHead(notifications) => {
                notifications.poll_next_unpin(cx).map(|result| result.map(Ok))
            }
            ExExNotificationsInner::WithHead(notifications) => notifications.poll_next_unpin(cx),
            ExExNotificationsInner::Invalid => unreachable!(),
        }
    }
}

/// A stream of [`ExExNotification`]s. The stream will emit notifications for all blocks.
pub struct ExExNotificationsWithoutHead<P> {
    node_head: BlockNumHash,
    provider: P,
    evm_config: BaseEvmConfig,
    notifications: Receiver<ExExNotification>,
    wal_handle: WalHandle,
}

impl<P: Debug> Debug for ExExNotificationsWithoutHead<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExExNotifications")
            .field("provider", &self.provider)
            .field("evm_config", &self.evm_config)
            .field("notifications", &self.notifications)
            .finish()
    }
}

impl<P> ExExNotificationsWithoutHead<P> {
    /// Creates a new instance of [`ExExNotificationsWithoutHead`].
    const fn new(
        node_head: BlockNumHash,
        provider: P,
        evm_config: BaseEvmConfig,
        notifications: Receiver<ExExNotification>,
        wal_handle: WalHandle,
    ) -> Self {
        Self { node_head, provider, evm_config, notifications, wal_handle }
    }

    /// Subscribe to notifications with the given head.
    /// Return this stream configured with the provided head.
    pub fn with_head(self, head: ExExHead) -> ExExNotificationsWithHead<P> {
        ExExNotificationsWithHead::new(
            self.node_head,
            self.provider,
            self.evm_config,
            self.notifications,
            self.wal_handle,
            head,
        )
    }
}

impl<P: Unpin> Stream for ExExNotificationsWithoutHead<P> {
    type Item = ExExNotification;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().notifications.poll_recv(cx)
    }
}

/// A stream of [`ExExNotification`]s. The stream will only emit notifications for blocks that are
/// committed or reverted after the given head. The head is the ExEx's latest view of the host
/// chain.
///
/// Notifications will be sent starting from the head, not inclusive. For example, if
/// `exex_head.number == 10`, then the first notification will be with `block.number == 11`. An
/// `exex_head.number` of 10 indicates that the ExEx has processed up to block 10, and is ready to
/// process block 11.
#[derive(Debug)]
pub struct ExExNotificationsWithHead<P> {
    /// The node's local head at launch.
    initial_local_head: BlockNumHash,
    provider: P,
    evm_config: BaseEvmConfig,
    notifications: Receiver<ExExNotification>,
    wal_handle: WalHandle,
    /// The exex head at launch
    initial_exex_head: ExExHead,

    /// If true, then we need to check if the ExEx head is on the canonical chain and if not,
    /// revert its head.
    pending_check_canonical: bool,
    /// If true, then we need to check if the ExEx head is behind the node head and if so, backfill
    /// the missing blocks.
    pending_check_backfill: bool,
    /// The backfill job to run before consuming any notifications.
    backfill_job: Option<StreamBackfillJob<P, Chain>>,
    /// Notifications that arrived during backfill and need to be delivered after it completes.
    /// These are notifications for blocks beyond the backfill range that we must not drop.
    pending_notifications: VecDeque<ExExNotification>,
}

impl<P> ExExNotificationsWithHead<P> {
    /// Creates a new [`ExExNotificationsWithHead`].
    const fn new(
        node_head: BlockNumHash,
        provider: P,
        evm_config: BaseEvmConfig,
        notifications: Receiver<ExExNotification>,
        wal_handle: WalHandle,
        exex_head: ExExHead,
    ) -> Self {
        Self {
            initial_local_head: node_head,
            provider,
            evm_config,
            notifications,
            wal_handle,
            initial_exex_head: exex_head,
            pending_check_canonical: true,
            pending_check_backfill: true,
            backfill_job: None,
            pending_notifications: VecDeque::new(),
        }
    }
}

impl<P> ExExNotificationsWithHead<P>
where
    P: BlockReader + HeaderProvider + StateProviderFactory + Clone + Unpin + 'static,
{
    /// Checks if the ExEx head is on the canonical chain.
    ///
    /// If the head block is not found in the database or it's ahead of the node head, it means
    /// we're not on the canonical chain and we need to revert the notification with the ExEx
    /// head block.
    fn check_canonical(&mut self) -> eyre::Result<Option<ExExNotification>> {
        if self.provider.is_known(self.initial_exex_head.block.hash)?
            && self.initial_exex_head.block.number <= self.initial_local_head.number
        {
            // we have the targeted block and that block is below the current head
            debug!(target: "exex::notifications", "ExEx head is on the canonical chain");
            return Ok(None);
        }

        // If the head block is not found in the database, it means we're not on the canonical
        // chain.

        // Get the committed notification for the head block from the WAL.
        let Some(notification) = self
            .wal_handle
            .get_committed_notification_by_block_hash(&self.initial_exex_head.block.hash)?
        else {
            // it's possible that the exex head is further ahead
            if self.initial_exex_head.block.number > self.initial_local_head.number {
                debug!(target: "exex::notifications", "ExEx head is ahead of the canonical chain");
                return Ok(None);
            }

            return Err(eyre::eyre!(
                "Could not find notification for block hash {:?} in the WAL",
                self.initial_exex_head.block.hash
            ));
        };

        // Update the head block hash to the parent hash of the first committed block.
        let committed_chain = notification.committed_chain().unwrap();
        let new_exex_head =
            (committed_chain.first().parent_hash(), committed_chain.first().number() - 1).into();
        debug!(target: "exex::notifications", old_exex_head = ?self.initial_exex_head.block, new_exex_head = ?new_exex_head, "ExEx head updated");
        self.initial_exex_head.block = new_exex_head;

        // Return an inverted notification. See the documentation for
        // `ExExNotification::into_inverted`.
        Ok(Some(notification.into_inverted()))
    }

    /// Compares the node head against the ExEx head, and backfills if needed.
    ///
    /// CAUTION: This method assumes that the ExEx head is <= the node head, and that it's on the
    /// canonical chain.
    ///
    /// Possible situations are:
    /// - ExEx is behind the node head (`exex_head.number < node_head.number`). Backfill from the
    ///   node database.
    /// - ExEx is at the same block number as the node head (`exex_head.number ==
    ///   node_head.number`). Nothing to do.
    fn check_backfill(&mut self) -> eyre::Result<()> {
        let backfill_job_factory =
            BackfillJobFactory::new(self.evm_config.clone(), self.provider.clone());
        match self.initial_exex_head.block.number.cmp(&self.initial_local_head.number) {
            std::cmp::Ordering::Less => {
                // ExEx is behind the node head, start backfill
                debug!(target: "exex::notifications", "ExEx is behind the node head and on the canonical chain, starting backfill");
                let backfill = backfill_job_factory
                    .backfill(
                        self.initial_exex_head.block.number + 1..=self.initial_local_head.number,
                    )
                    .into_stream();
                self.backfill_job = Some(backfill);
            }
            std::cmp::Ordering::Equal => {
                debug!(target: "exex::notifications", "ExEx is at the node head");
            }
            std::cmp::Ordering::Greater => {
                debug!(target: "exex::notifications", "ExEx is ahead of the node head");
            }
        };

        Ok(())
    }
}

impl<P> Stream for ExExNotificationsWithHead<P>
where
    P: BlockReader + HeaderProvider + StateProviderFactory + Clone + Unpin + 'static,
{
    type Item = eyre::Result<ExExNotification>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // 1. Check once whether we need to retrieve a notification gap from the WAL.
        if this.pending_check_canonical {
            if let Some(canonical_notification) = this.check_canonical()? {
                return Poll::Ready(Some(Ok(canonical_notification)));
            }

            // ExEx head is on the canonical chain, we no longer need to check it
            this.pending_check_canonical = false;
        }

        // 2. Check once whether we need to trigger backfill sync
        if this.pending_check_backfill {
            this.check_backfill()?;
            this.pending_check_backfill = false;
        }

        // 3. If backfill is in progress yield new notifications
        if let Some(backfill_job) = &mut this.backfill_job {
            debug!(target: "exex::notifications", "Polling backfill job");

            // Drain the notification channel to prevent backpressure from stalling the
            // ExExManager. During backfill, the ExEx is not consuming from the channel,
            // so the capacity-1 channel fills up, which blocks the manager's PollSender,
            // which fills the manager's 1024-entry buffer, which blocks all upstream
            // senders. Notifications for blocks covered by the backfill range are
            // discarded (they'll be re-delivered by the backfill job), while
            // notifications beyond the backfill range are buffered for delivery after the
            // backfill completes.
            while let Poll::Ready(Some(notification)) = this.notifications.poll_recv(cx) {
                // Always buffer revert-containing notifications (ChainReverted,
                // ChainReorged) because the backfill job only re-delivers
                // ChainCommitted from the database. Discarding a reorg here would
                // leave the ExEx unaware of the fork switch.
                if notification.reverted_chain().is_some() {
                    this.pending_notifications.push_back(notification);
                    continue;
                }
                if let Some(committed) = notification.committed_chain()
                    && committed.tip().number() <= this.initial_local_head.number
                {
                    // Covered by backfill range, safe to discard
                    continue;
                }
                // Beyond the backfill range — buffer for delivery after backfill
                this.pending_notifications.push_back(notification);
            }

            if let Some(chain) = ready!(backfill_job.poll_next_unpin(cx)).transpose()? {
                debug!(target: "exex::notifications", range = ?chain.range(), "Backfill job returned a chain");
                return Poll::Ready(Some(Ok(ExExNotification::ChainCommitted {
                    new: Arc::new(chain),
                })));
            }

            // Backfill job is done, remove it
            this.backfill_job = None;
        }

        // 4. Deliver any notifications that were buffered during backfill
        if let Some(notification) = this.pending_notifications.pop_front() {
            return Poll::Ready(Some(Ok(notification)));
        }

        // 5. Otherwise advance the regular event stream
        loop {
            let Some(notification) = ready!(this.notifications.poll_recv(cx)) else {
                return Poll::Ready(None);
            };

            // 6. In case the exex is ahead of the new tip, we must skip it
            if let Some(committed) = notification.committed_chain() {
                // inclusive check because we should start with `exex.head + 1`
                if this.initial_exex_head.block.number >= committed.tip().number() {
                    continue;
                }
            }

            return Poll::Ready(Some(Ok(notification)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_eips::BlockNumHash;
    use base_common_types_chain::{BaseBlock, Header};
    use base_execution_state_operations::init::init_genesis;
    use base_execution_state_provider::{
        BlockWriter, Chain, DBProvider, DatabaseProviderFactory, providers::BlockchainProvider,
        test_utils::create_test_provider_factory,
    };
    use base_testing_support::{generators, generators::BlockParams};
    use eyre::OptionExt;
    use futures::StreamExt;
    use tokio::sync::mpsc;

    use super::*;
    use crate::Wal;

    #[tokio::test]
    async fn exex_notifications_behind_head_canonical() -> eyre::Result<()> {
        let mut rng = generators::rng();

        let temp_dir = tempfile::tempdir().unwrap();
        let wal = Wal::new(temp_dir.path()).unwrap();

        let provider_factory = create_test_provider_factory();
        let genesis_hash = init_genesis(&provider_factory)?;
        let genesis_block = provider_factory
            .block(genesis_hash.into())?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?;

        let provider = BlockchainProvider::new(provider_factory.clone())?;

        let node_head_block = base_testing_support::BaseTestData::random_block(
            &mut rng,
            genesis_block.number + 1,
            BlockParams { parent: Some(genesis_hash), tx_count: Some(0), ..Default::default() },
        )
        .try_recover()?;
        let node_head = node_head_block.num_hash();
        let provider_rw = provider_factory.provider_rw()?;
        provider_rw.insert_block(&node_head_block)?;
        provider_rw.commit()?;
        let exex_head =
            ExExHead { block: BlockNumHash { number: genesis_block.number, hash: genesis_hash } };

        let notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    base_testing_support::BaseTestData::random_block(
                        &mut rng,
                        node_head.number + 1,
                        BlockParams { parent: Some(node_head.hash), ..Default::default() },
                    )
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };

        let (notifications_tx, notifications_rx) = mpsc::channel(1);

        notifications_tx.send(notification.clone()).await?;

        let mut notifications = ExExNotificationsWithoutHead::new(
            node_head,
            provider,
            BaseEvmConfig::default(),
            notifications_rx,
            wal.handle(),
        )
        .with_head(exex_head);

        // First notification is the backfill of missing blocks from the canonical chain
        assert_eq!(
            notifications.next().await.transpose()?,
            Some(ExExNotification::ChainCommitted {
                new: Arc::new(
                    BackfillJobFactory::new(
                        notifications.evm_config.clone(),
                        notifications.provider.clone()
                    )
                    .backfill(1..=1)
                    .next()
                    .ok_or_eyre("failed to backfill")??
                )
            })
        );

        // Second notification is the actual notification that we sent before
        assert_eq!(notifications.next().await.transpose()?, Some(notification));

        Ok(())
    }

    #[tokio::test]
    async fn catch_up_with_head_after_pause_backfills_missed_blocks() -> eyre::Result<()> {
        let mut rng = generators::rng();

        let temp_dir = tempfile::tempdir().unwrap();
        let wal = Wal::new(temp_dir.path()).unwrap();

        let provider_factory = create_test_provider_factory();
        let genesis_hash = init_genesis(&provider_factory)?;
        let genesis_block = provider_factory
            .block(genesis_hash.into())?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?;
        let provider = BlockchainProvider::new(provider_factory.clone())?;

        let exex_head =
            ExExHead { block: BlockNumHash { number: genesis_block.number, hash: genesis_hash } };
        let (notifications_tx, notifications_rx) = mpsc::channel(1);

        let evm_config = BaseEvmConfig::default();
        let mut notifications = ExExNotifications::new(
            BlockNumHash { number: genesis_block.number, hash: genesis_hash },
            provider.clone(),
            evm_config.clone(),
            notifications_rx,
            wal.handle(),
        );
        // The ExEx configures its head at launch, as usual.
        notifications.set_with_head(exex_head);

        // Block 1 is delivered live and consumed, but the ExEx fails to durably process it.
        let node_head_block = base_testing_support::BaseTestData::random_block(
            &mut rng,
            genesis_block.number + 1,
            BlockParams { parent: Some(genesis_hash), tx_count: Some(0), ..Default::default() },
        )
        .try_recover()?;
        let node_head = node_head_block.num_hash();
        let block_1_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![node_head_block.clone()],
                Default::default(),
                BTreeMap::new(),
            )),
        };
        notifications_tx.send(block_1_notification.clone()).await?;
        assert_eq!(notifications.next().await.transpose()?, Some(block_1_notification));

        // Meanwhile the node commits block 1 and advances its canonical head past the
        // launch-time head.
        let provider_rw = provider_factory.provider_rw()?;
        provider_rw.insert_block(&node_head_block)?;
        provider_rw.commit()?;
        provider
            .canonical_in_memory_state()
            .set_canonical_head(node_head_block.clone_sealed_header());

        // The ExEx recovers still at genesis and catches up, it needs block 1 again, but that
        // notification is long gone from the channel.
        notifications.catch_up_with_head(exex_head)?;

        let block_2_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    base_testing_support::BaseTestData::random_block(
                        &mut rng,
                        node_head.number + 1,
                        BlockParams { parent: Some(node_head.hash), ..Default::default() },
                    )
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };
        notifications_tx.send(block_2_notification.clone()).await?;

        // Backfill re-delivers block 1 up to the node's current head
        assert_eq!(
            notifications.next().await.transpose()?,
            Some(ExExNotification::ChainCommitted {
                new: Arc::new(
                    BackfillJobFactory::new(evm_config, provider)
                        .backfill(1..=1)
                        .next()
                        .ok_or_eyre("failed to backfill")??
                )
            })
        );
        // followed by the live notification for block 2.
        assert_eq!(notifications.next().await.transpose()?, Some(block_2_notification));

        Ok(())
    }

    #[tokio::test]
    async fn exex_notifications_same_head_canonical() -> eyre::Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = Wal::new(temp_dir.path()).unwrap();

        let provider_factory = create_test_provider_factory();
        let genesis_hash = init_genesis(&provider_factory)?;
        let genesis_block = provider_factory
            .block(genesis_hash.into())?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?;

        let provider = BlockchainProvider::new(provider_factory)?;

        let node_head = BlockNumHash { number: genesis_block.number, hash: genesis_hash };
        let exex_head = ExExHead { block: node_head };

        let notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    BaseBlock {
                        header: Header {
                            parent_hash: node_head.hash,
                            number: node_head.number + 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                    .seal_slow()
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };

        let (notifications_tx, notifications_rx) = mpsc::channel(1);

        notifications_tx.send(notification.clone()).await?;

        let mut notifications = ExExNotificationsWithoutHead::new(
            node_head,
            provider,
            BaseEvmConfig::default(),
            notifications_rx,
            wal.handle(),
        )
        .with_head(exex_head);

        let new_notification = notifications.next().await.transpose()?;
        assert_eq!(new_notification, Some(notification));

        Ok(())
    }

    #[tokio::test]
    async fn exex_notifications_same_head_non_canonical() -> eyre::Result<()> {
        let mut rng = generators::rng();

        let temp_dir = tempfile::tempdir().unwrap();
        let wal = Wal::new(temp_dir.path()).unwrap();

        let provider_factory = create_test_provider_factory();
        let genesis_hash = init_genesis(&provider_factory)?;
        let genesis_block = provider_factory
            .block(genesis_hash.into())?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?;

        let provider = BlockchainProvider::new(provider_factory)?;

        let node_head_block = base_testing_support::BaseTestData::random_block(
            &mut rng,
            genesis_block.number + 1,
            BlockParams { parent: Some(genesis_hash), tx_count: Some(0), ..Default::default() },
        )
        .try_recover()?;
        let node_head = node_head_block.num_hash();
        let provider_rw = provider.database_provider_rw()?;
        provider_rw.insert_block(&node_head_block)?;
        provider_rw.commit()?;
        let node_head_notification = ExExNotification::ChainCommitted {
            new: Arc::new(
                BackfillJobFactory::new(BaseEvmConfig::default(), provider.clone())
                    .backfill(node_head.number..=node_head.number)
                    .next()
                    .ok_or_else(|| eyre::eyre!("failed to backfill"))??,
            ),
        };

        let exex_head_block = base_testing_support::BaseTestData::random_block(
            &mut rng,
            genesis_block.number + 1,
            BlockParams { parent: Some(genesis_hash), tx_count: Some(0), ..Default::default() },
        );
        let exex_head = ExExHead { block: exex_head_block.num_hash() };
        let exex_head_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![exex_head_block.clone().try_recover()?],
                Default::default(),
                BTreeMap::new(),
            )),
        };
        wal.commit(&exex_head_notification)?;

        let new_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    base_testing_support::BaseTestData::random_block(
                        &mut rng,
                        node_head.number + 1,
                        BlockParams { parent: Some(node_head.hash), ..Default::default() },
                    )
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };

        let (notifications_tx, notifications_rx) = mpsc::channel(1);

        notifications_tx.send(new_notification.clone()).await?;

        let mut notifications = ExExNotificationsWithoutHead::new(
            node_head,
            provider,
            BaseEvmConfig::default(),
            notifications_rx,
            wal.handle(),
        )
        .with_head(exex_head);

        // First notification is the revert of the ExEx head block to get back to the canonical
        // chain
        assert_eq!(
            notifications.next().await.transpose()?,
            Some(exex_head_notification.into_inverted())
        );
        // Second notification is the backfilled block from the canonical chain to get back to the
        // canonical tip
        assert_eq!(notifications.next().await.transpose()?, Some(node_head_notification));
        // Third notification is the actual notification that we sent before
        assert_eq!(notifications.next().await.transpose()?, Some(new_notification));

        Ok(())
    }

    #[tokio::test]
    async fn test_notifications_ahead_of_head() -> eyre::Result<()> {
        base_common_observability_tracing::init_test_tracing();
        let mut rng = generators::rng();

        let temp_dir = tempfile::tempdir().unwrap();
        let wal = Wal::new(temp_dir.path()).unwrap();

        let provider_factory = create_test_provider_factory();
        let genesis_hash = init_genesis(&provider_factory)?;
        let genesis_block = provider_factory
            .block(genesis_hash.into())?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?;

        let provider = BlockchainProvider::new(provider_factory)?;

        let exex_head_block = base_testing_support::BaseTestData::random_block(
            &mut rng,
            genesis_block.number + 1,
            BlockParams { parent: Some(genesis_hash), tx_count: Some(0), ..Default::default() },
        );
        let exex_head_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![exex_head_block.clone().try_recover()?],
                Default::default(),
                BTreeMap::new(),
            )),
        };
        wal.commit(&exex_head_notification)?;

        let node_head = BlockNumHash { number: genesis_block.number, hash: genesis_hash };
        let exex_head = ExExHead {
            block: BlockNumHash { number: exex_head_block.number, hash: exex_head_block.hash() },
        };

        let new_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    base_testing_support::BaseTestData::random_block(
                        &mut rng,
                        genesis_block.number + 1,
                        BlockParams { parent: Some(genesis_hash), ..Default::default() },
                    )
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };

        let (notifications_tx, notifications_rx) = mpsc::channel(1);

        notifications_tx.send(new_notification.clone()).await?;

        let mut notifications = ExExNotificationsWithoutHead::new(
            node_head,
            provider,
            BaseEvmConfig::default(),
            notifications_rx,
            wal.handle(),
        )
        .with_head(exex_head);

        // First notification is the revert of the ExEx head block to get back to the canonical
        // chain
        assert_eq!(
            notifications.next().await.transpose()?,
            Some(exex_head_notification.into_inverted())
        );

        // Second notification is the actual notification that we sent before
        assert_eq!(notifications.next().await.transpose()?, Some(new_notification));

        Ok(())
    }

    /// Regression test for <https://github.com/paradigmxyz/reth/issues/19665>.
    ///
    /// During backfill, `poll_next` must drain the notification channel so that
    /// the upstream `ExExManager` is never blocked by a full channel. Without
    /// the drain loop the capacity-1 channel stays full for the entire backfill
    /// duration, which stalls the manager's `PollSender` and eventually blocks
    /// all upstream senders once the 1024-entry buffer fills up.
    ///
    /// The key assertion is the `try_send` after the first `poll_next`: it
    /// proves the channel was drained during the backfill poll. Without the
    /// fix this `try_send` fails because the notification is still sitting in
    /// the channel.
    #[tokio::test]
    async fn exex_notifications_backfill_drains_channel() -> eyre::Result<()> {
        let mut rng = generators::rng();

        let temp_dir = tempfile::tempdir().unwrap();
        let wal = Wal::new(temp_dir.path()).unwrap();

        let provider_factory = create_test_provider_factory();
        let genesis_hash = init_genesis(&provider_factory)?;
        let genesis_block = provider_factory
            .block(genesis_hash.into())?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?;

        let provider = BlockchainProvider::new(provider_factory.clone())?;

        // Insert block 1 into the DB so there's something to backfill
        let node_head_block = base_testing_support::BaseTestData::random_block(
            &mut rng,
            genesis_block.number + 1,
            BlockParams { parent: Some(genesis_hash), tx_count: Some(0), ..Default::default() },
        )
        .try_recover()?;
        let node_head = node_head_block.num_hash();
        let provider_rw = provider_factory.provider_rw()?;
        provider_rw.insert_block(&node_head_block)?;
        provider_rw.commit()?;

        // ExEx head is at genesis — backfill will run for block 1
        let exex_head =
            ExExHead { block: BlockNumHash { number: genesis_block.number, hash: genesis_hash } };

        // Notification for a block AFTER the backfill range (block 2).
        let post_backfill_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    base_testing_support::BaseTestData::random_block(
                        &mut rng,
                        node_head.number + 1,
                        BlockParams { parent: Some(node_head.hash), ..Default::default() },
                    )
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };

        // Another notification (block 3) used to probe channel capacity.
        let probe_notification = ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                vec![
                    base_testing_support::BaseTestData::random_block(
                        &mut rng,
                        node_head.number + 2,
                        BlockParams { parent: None, ..Default::default() },
                    )
                    .try_recover()?,
                ],
                Default::default(),
                BTreeMap::new(),
            )),
        };

        let (notifications_tx, notifications_rx) = mpsc::channel(1);

        // Fill the capacity-1 channel.
        notifications_tx.send(post_backfill_notification.clone()).await?;

        // Confirm the channel is full — this is the precondition that causes the
        // stall in production: the ExExManager's PollSender would block here.
        assert!(
            notifications_tx.try_send(probe_notification.clone()).is_err(),
            "channel should be full before backfill poll"
        );

        let mut notifications = ExExNotificationsWithoutHead::new(
            node_head,
            provider,
            BaseEvmConfig::default(),
            notifications_rx,
            wal.handle(),
        )
        .with_head(exex_head);

        // Poll once — this returns the backfill result for block 1. Crucially,
        // the drain loop in poll_next runs in this same call, consuming the
        // notification from the channel and buffering it.
        let backfill_result = notifications.next().await.transpose()?;
        assert_eq!(
            backfill_result,
            Some(ExExNotification::ChainCommitted {
                new: Arc::new(
                    BackfillJobFactory::new(
                        notifications.evm_config.clone(),
                        notifications.provider.clone()
                    )
                    .backfill(1..=1)
                    .next()
                    .ok_or_eyre("failed to backfill")??
                )
            })
        );

        // KEY ASSERTION: the channel was drained during the backfill poll above.
        // Without the drain loop this try_send fails because the original
        // notification is still occupying the capacity-1 channel.
        assert!(
            notifications_tx.try_send(probe_notification.clone()).is_ok(),
            "channel should have been drained during backfill poll"
        );

        // The first buffered notification (block 2) was drained from the channel
        // during backfill and is delivered now.
        let buffered = notifications.next().await.transpose()?;
        assert_eq!(buffered, Some(post_backfill_notification));

        // The probe notification (block 3) that we just sent is delivered next.
        let probe = notifications.next().await.transpose()?;
        assert_eq!(probe, Some(probe_notification));

        Ok(())
    }
}
