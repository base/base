//! Support for launching execution extensions.

use std::{fmt, fmt::Debug};

use alloy_eips::{BlockNumHash, eip2124::Head};
use base_common_observability_tracing::tracing::{debug, info};
use base_execution_engine_observers::{
    DEFAULT_EXEX_MANAGER_CAPACITY, DEFAULT_WAL_BLOCKS_WARNING, ExExContext, ExExHandle,
    ExExManager, ExExManagerHandle, ExExNotificationSource, Wal,
};
use base_execution_state_provider::CanonStateSubscriptions;
use base_execution_state_provider::ForkChoiceSubscriptions;
use futures::future;
use tracing::Instrument;

use crate::WithConfigs;

/// Can launch execution extensions.
pub struct ExExLauncher {
    head: Head,
    extensions: Vec<crate::BaseExecutionService>,
    components: crate::BaseNodeContext,
    config_container: WithConfigs,
    /// The threshold for the number of blocks in the WAL before emitting a warning.
    wal_blocks_warning: usize,
    /// The max notification buffer capacity for the ExEx manager.
    capacity: usize,
}

impl ExExLauncher {
    /// Create a new `ExExLauncher` with the given extensions.
    pub const fn new(
        head: Head,
        components: crate::BaseNodeContext,
        extensions: Vec<crate::BaseExecutionService>,
        config_container: WithConfigs,
    ) -> Self {
        Self {
            head,
            extensions,
            components,
            config_container,
            wal_blocks_warning: DEFAULT_WAL_BLOCKS_WARNING,
            capacity: DEFAULT_EXEX_MANAGER_CAPACITY,
        }
    }

    /// Sets the threshold for the number of blocks in the WAL before emitting a warning.
    ///
    /// For L2 chains with faster block times, this value should be increased proportionally
    /// to avoid excessive warnings. For example, a chain with 2-second block times might use
    /// a value 6x higher than the default (768 instead of 128).
    pub const fn with_wal_blocks_warning(mut self, threshold: usize) -> Self {
        self.wal_blocks_warning = threshold;
        self
    }

    /// Sets the max notification buffer capacity for the [`ExExManager`].
    pub const fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Launches all execution extensions.
    ///
    /// Spawns all extensions and returns the handle to the exex manager if any extensions are
    /// installed.
    pub async fn launch(self) -> eyre::Result<Option<ExExManagerHandle>> {
        let Self { head, extensions, components, config_container, wal_blocks_warning, capacity } =
            self;
        let head = BlockNumHash::new(head.number, head.hash);

        if extensions.is_empty() {
            // nothing to launch
            return Ok(None);
        }

        info!(target: "reth::cli", "Loading ExEx Write-Ahead Log...");
        let exex_wal = Wal::new(
            config_container
                .config
                .datadir
                .clone()
                .resolve_datadir(config_container.config.chain.chain())
                .exex_wal(),
        )?;

        let mut exex_handles = Vec::with_capacity(extensions.len());
        let mut exexes = Vec::with_capacity(extensions.len());

        for exex in extensions {
            let id = exex.id().to_string();
            // create a new exex handle
            let (handle, events, notifications) = ExExHandle::new(
                id.clone(),
                head,
                components.provider().clone(),
                components.evm_config().clone(),
                exex_wal.handle(),
            );
            exex_handles.push(handle);

            // create the launch context for the exex
            let context = ExExContext {
                head,
                provider: components.provider().clone(),
                evm_config: components.evm_config().clone(),
                task_executor: components.task_executor().clone(),
                network: components.network().clone(),
                events,
                notifications,
            };

            let executor = components.task_executor().clone();
            exexes.push(async move {
                debug!(target: "reth::cli", id, "spawning exex");
                let span = base_common_observability_tracing::tracing::info_span!("exex", id);

                // init the exex
                let exex = exex.run(context);

                // spawn it as a crit task
                executor.spawn_critical_task(
                    "exex",
                    async move {
                        info!(target: "reth::cli", "ExEx started");
                        match exex.await {
                            Ok(_) => panic!("ExEx {id} finished. ExExes should run indefinitely"),
                            Err(err) => panic!("ExEx {id} crashed: {err}"),
                        }
                    }
                    .instrument(span),
                );

                Ok::<(), eyre::Error>(())
            });
        }

        future::try_join_all(exexes).await?;

        // spawn exex manager
        debug!(target: "reth::cli", "spawning exex manager");
        let exex_manager = ExExManager::new(
            components.provider().clone(),
            exex_handles,
            capacity,
            exex_wal,
            components.provider().finalized_block_stream(),
        )
        .with_wal_blocks_warning(wal_blocks_warning);
        let exex_manager_handle = exex_manager.handle();
        components.task_executor().spawn_critical_task("exex manager", async move {
            exex_manager.await.expect("exex manager crashed");
        });

        // send notifications from the blockchain tree to exex manager
        let mut canon_state_notifications = components.provider().subscribe_to_canonical_state();
        let mut handle = exex_manager_handle.clone();
        components.task_executor().spawn_critical_task(
            "exex manager blockchain tree notifications",
            async move {
                while let Ok(notification) = canon_state_notifications.recv().await {
                    handle
                        .send_async(ExExNotificationSource::BlockchainTree, notification.into())
                        .await
                        .expect("blockchain tree notification could not be sent to exex manager");
                }
            },
        );

        info!(target: "reth::cli", "ExEx Manager started");

        Ok(Some(exex_manager_handle))
    }
}

impl Debug for ExExLauncher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExExLauncher")
            .field("head", &self.head)
            .field(
                "extensions",
                &self.extensions.iter().map(crate::BaseExecutionService::id).collect::<Vec<_>>(),
            )
            .field("components", &"...")
            .field("config_container", &self.config_container)
            .field("wal_blocks_warning", &self.wal_blocks_warning)
            .finish()
    }
}
