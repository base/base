//! Node extension that installs the archive-backed `eth_` reads behind `--archive.url`.

use std::{sync::Arc, time::Duration};

use base_execution_eip8130_rpc::Eip8130EthApiExt;
use base_history_store::{CacheConfig, IndexOptions, ReaderConfig};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_prune_types::PruneMode;
use tracing::{info, warn};

use crate::{
    ArchiveConverter, ArchiveEth, ArchiveEthApiServer, ArchiveHandle, ArchiveSource, RetentionCache,
};

/// Default bound on the startup catalog check.
pub const DEFAULT_STARTUP_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration of [`ArchiveEthExtension`].
#[derive(Debug, Clone)]
pub struct ArchiveEthConfig {
    /// Archive location.
    pub source: ArchiveSource,
    /// Segment cache sizing.
    pub cache: CacheConfig,
    /// Index reader options.
    pub index: IndexOptions,
    /// Bound on the startup catalog check; on timeout the node starts and stays lazy.
    pub startup_check_timeout: Duration,
    /// How long a resolved archive snapshot is reused, and how long archive reads fail fast
    /// after the reader failed to open.
    pub snapshot_ttl: Duration,
}

/// Replaces the node's block, transaction, receipt, log and state reads with local-first,
/// archive-on-miss versions.
///
/// It owns `eth_getTransactionCount` too, so the EIP-8130 extension must be installed with
/// `TransactionCountOwner::Archive` whenever this one is installed.
#[derive(Debug)]
pub struct ArchiveEthExtension {
    /// Configuration; `None` leaves the node unchanged.
    pub config: Option<ArchiveEthConfig>,
}

impl BaseNodeExtension for ArchiveEthExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let Some(config) = self.config else { return hooks };
        hooks.add_rpc_module(move |ctx| {
            let provider = ctx.provider().clone();
            let chain_spec = provider.chain_spec();
            info!(
                prefix = %config.source.layout.prefix,
                memory_segments = config.cache.memory_segments,
                disk_cache = config.cache.disk_dir.is_some(),
                "starting archive-backed eth RPC"
            );
            let reader_config = ReaderConfig {
                chain_id: chain_spec.chain().id(),
                genesis_hash: chain_spec.genesis_hash(),
                chain_spec: Arc::clone(&chain_spec),
                cache: config.cache,
                index: config.index,
                snapshot_ttl: config.snapshot_ttl,
            };
            let archive = Arc::new(ArchiveHandle::new(config.source, reader_config));
            let state_retention = ctx.config().prune_config().and_then(|prune| {
                [prune.segments.account_history, prune.segments.storage_history]
                    .into_iter()
                    .flatten()
                    .filter_map(|mode| match mode {
                        PruneMode::Full => Some(0),
                        PruneMode::Distance(distance) => Some(distance),
                        PruneMode::Before(_) => None,
                    })
                    .min()
            });
            Self::check_startup(&archive, config.startup_check_timeout, state_retention)?;
            let max_logs_per_response =
                ctx.config().rpc.rpc_max_logs_per_response.unwrap_or_max() as usize;
            let eth_api = ctx.registry.eth_api().clone();
            let api = ArchiveEth {
                eth: eth_api.clone(),
                filter: ctx.registry.eth_handlers().filter.clone(),
                eip8130: Eip8130EthApiExt::new(eth_api),
                provider,
                archive,
                converter: ArchiveConverter::new(chain_spec),
                max_logs_per_response,
                retention: RetentionCache::new(config.snapshot_ttl),
            };
            ctx.modules.replace_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl ArchiveEthExtension {
    /// Runs [`ArchiveHandle::check_identity`] within `timeout` and fails node start on a
    /// verified mismatch. The RPC hook is synchronous and may run inside the node's runtime, so
    /// the check runs on a scoped thread with its own current-thread runtime.
    pub fn check_startup(
        archive: &ArchiveHandle,
        timeout: Duration,
        state_retention: Option<u64>,
    ) -> eyre::Result<()> {
        let outcome = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime =
                        tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                    // The timer must be created inside the runtime it runs on.
                    Ok::<_, std::io::Error>(runtime.block_on(async {
                        tokio::time::timeout(timeout, archive.check_identity(state_retention)).await
                    }))
                })
                .join()
        });
        match outcome {
            Ok(Ok(Ok(Ok(())))) => Ok(()),
            Ok(Ok(Ok(Err(err)))) => Err(eyre::eyre!("refusing foreign archive: {err}")),
            Ok(Ok(Err(_elapsed))) => {
                warn!(?timeout, "archive startup check timed out; serving local data");
                Ok(())
            }
            Ok(Err(err)) => Err(err.into()),
            Err(_) => Err(eyre::eyre!("archive startup check panicked")),
        }
    }
}

impl FromExtensionConfig for ArchiveEthExtension {
    type Config = Option<ArchiveEthConfig>;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
