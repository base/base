use std::{fmt::Debug, sync::Arc};

use alloy_eips::BlockNumHash;
use base_common_chain_config::{BaseChainSpec, ChainSpecProvider};
use base_common_runtime::Runtime;
use base_common_types_chain::RecoveredBlock;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_network_service::{
    NetworkConfigBuilder, NetworkManager, config::rng_secret_key,
};
use base_execution_state_database::test_utils::{
    create_test_rocksdb_dir, create_test_rw_db, create_test_static_files_dir,
};
use base_execution_state_operations::init::init_genesis;
use base_execution_state_provider::{
    BlockReader, ProviderFactory,
    providers::{BlockchainProvider, RocksDBProvider, StaticFileProvider},
};
use base_execution_state_types::Chain;
use tempfile::TempDir;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};

use crate::{ExExContext, ExExEvent, ExExNotification, ExExNotifications, Wal};

/// A helper type for testing Execution Extensions.
#[derive(Debug)]
pub struct TestExExHandle {
    /// Genesis block that was inserted into the storage
    pub genesis: RecoveredBlock,
    /// Provider Factory for accessing the emphemeral storage of the host node
    pub provider_factory: ProviderFactory,
    /// Channel for receiving events from the Execution Extension
    pub events_rx: UnboundedReceiver<ExExEvent>,
    /// Channel for sending notifications to the Execution Extension
    pub notifications_tx: Sender<ExExNotification>,
    /// Node task runtime
    pub runtime: Runtime,
    /// WAL temp directory handle
    _wal_directory: TempDir,
}

impl TestExExHandle {
    /// Send a notification to the Execution Extension that the chain has been committed
    pub async fn send_notification_chain_committed(&self, chain: Chain) -> eyre::Result<()> {
        self.notifications_tx
            .send(ExExNotification::ChainCommitted { new: Arc::new(chain) })
            .await?;
        Ok(())
    }

    /// Send a notification to the Execution Extension that the chain has been reorged
    pub async fn send_notification_chain_reorged(
        &self,
        old: Chain,
        new: Chain,
    ) -> eyre::Result<()> {
        self.notifications_tx
            .send(ExExNotification::ChainReorged { old: Arc::new(old), new: Arc::new(new) })
            .await?;
        Ok(())
    }

    /// Send a notification to the Execution Extension that the chain has been reverted
    pub async fn send_notification_chain_reverted(&self, chain: Chain) -> eyre::Result<()> {
        self.notifications_tx
            .send(ExExNotification::ChainReverted { old: Arc::new(chain) })
            .await?;
        Ok(())
    }

    /// Asserts that the Execution Extension did not emit any events.
    #[track_caller]
    pub fn assert_events_empty(&self) {
        assert!(self.events_rx.is_empty());
    }

    /// Asserts that the Execution Extension emitted a `FinishedHeight` event with the correct
    /// height.
    #[track_caller]
    pub fn assert_event_finished_height(&mut self, height: BlockNumHash) -> eyre::Result<()> {
        let event = self.events_rx.try_recv()?;
        assert_eq!(event, ExExEvent::FinishedHeight(height));
        Ok(())
    }
}

impl TestExExHandle {
    /// Creates a new [`ExExContext`].
    ///
    /// This is a convenience function that does the following:
    /// 1. Sets up an [`ExExContext`] with all dependencies.
    /// 2. Inserts the genesis block of the provided (chain spec)[`BaseChainSpec`] into the storage.
    /// 3. Creates a channel for receiving events from the Execution Extension.
    /// 4. Creates a channel for sending notifications to the Execution Extension.
    ///
    /// # Warning
    /// The genesis block is not sent to the notifications channel. The caller is responsible for
    /// doing this.
    pub async fn with_chain_spec(
        chain_spec: Arc<BaseChainSpec>,
    ) -> eyre::Result<(ExExContext, TestExExHandle)> {
        let (static_dir, _) = create_test_static_files_dir();
        let (rocksdb_dir, _) = create_test_rocksdb_dir();
        let db = create_test_rw_db();
        let provider_factory = ProviderFactory::new(
            db,
            chain_spec,
            StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
            RocksDBProvider::builder(rocksdb_dir.keep()).with_default_tables().build().unwrap(),
            base_common_runtime::Runtime::test(),
        )?;

        let genesis_hash = init_genesis(&provider_factory)?;
        let provider = BlockchainProvider::new(provider_factory.clone())?;

        let evm_config = BaseEvmConfig::new(provider_factory.chain_spec());
        let runtime = Runtime::test();

        let network_manager = NetworkManager::new(
            NetworkConfigBuilder::new(rng_secret_key(), runtime.clone())
                .with_unused_discovery_port()
                .with_unused_listener_port()
                .build(provider_factory.clone()),
        )
        .await?;
        let network = network_manager.handle().clone();
        let task_executor = runtime.clone();
        runtime.spawn_task(network_manager);

        let genesis = provider_factory
            .block_by_hash(genesis_hash)?
            .ok_or_else(|| eyre::eyre!("genesis block not found"))?
            .seal_slow()
            .try_recover()?;

        let head = genesis.num_hash();

        let wal_directory = tempfile::tempdir()?;
        let wal = Wal::new(wal_directory.path())?;

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (notifications_tx, notifications_rx) = tokio::sync::mpsc::channel(1);
        let notifications = ExExNotifications::new(
            head,
            provider.clone(),
            evm_config.clone(),
            notifications_rx,
            wal.handle(),
        );

        let ctx = ExExContext {
            head,
            events: events_tx,
            notifications,
            provider,
            evm_config,
            task_executor,
            network,
        };

        Ok((
            ctx,
            TestExExHandle {
                genesis,
                provider_factory,
                events_rx,
                notifications_tx,
                runtime,
                _wal_directory: wal_directory,
            },
        ))
    }

    /// Creates a new [`ExExContext`] with (mainnet)[`std::sync::Arc::new(base_common_chain_config::BaseChainSpec::mainnet())`] chain spec.
    ///
    /// For more information see [`Self::with_chain_spec`].
    pub async fn create() -> eyre::Result<(ExExContext, TestExExHandle)> {
        Self::with_chain_spec(std::sync::Arc::new(
            base_common_chain_config::BaseChainSpec::mainnet(),
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_test_context_creation() {
        let _ = TestExExHandle::create().await.unwrap();
    }
}
