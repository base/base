//! Node extension behind `--archive.exoware.url`: the archive-backed `eth_` reads and, with
//! `--archive.exoware.export`, the segment exporter.

use std::{sync::Arc, time::Duration};

use base_execution_base_tx_rpc::TxBaseEthApiExt;
use base_exoware_archive::{
    ArchiveError, ArchiveMetrics, BlockVerifier, ExowareArchive, ExowareStore, ProvisionOutcome,
    ReaderLimits, SegmentPublisher,
};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_rpc_eth_api::QueryLimits;
use reth_storage_api::DatabaseProviderFactory;
use tokio::sync::OnceCell;
use tracing::{error, info, warn};

use crate::{
    ArchiveConverter, ArchiveEth, ArchiveEthApiServer, ArchiveExecutor, ArchiveHandle,
    ArchiveProvider, ExowareArchiveArgs, ExowareHistory, ProviderSource, RetentionCache,
};

/// How long a retention read is reused and a failed reader open fails fast.
pub const RETENTION_TTL: Duration = Duration::from_secs(2);

/// Replaces the node's block, transaction, receipt, log and state reads with local-first,
/// archive-on-prune versions, and runs the exporter when asked to.
///
/// It re-registers `eth_getTransactionCount` and serves every local call (and every non-zero
/// `nonce_key`) through the Base transaction override, so it must be installed after
/// `TxBaseRpcExtension`.
#[derive(Debug)]
pub struct ExowareArchiveExtension {
    /// Arguments; an unset url leaves the node unchanged.
    pub args: ExowareArchiveArgs,
}

impl ExowareArchiveExtension {
    /// Opens the configured namespace.
    pub fn store(args: &ExowareArchiveArgs, url: &str) -> eyre::Result<Arc<ExowareStore>> {
        Ok(Arc::new(ExowareStore::connect(
            url,
            &args.archive_exoware_namespace,
            args.archive_exoware_timeout,
        )?))
    }

    /// Runs the exporter until `shutdown`: provisions the config row when asked, opens the
    /// publisher (which requires the row to match) and publishes finalized segments. Shutdown
    /// is checked before provisioning, raced against every store call while opening, and
    /// inside every publication round (see `SegmentPublisher::run_until`).
    pub async fn export<P: ArchiveProvider>(
        store: Arc<ExowareStore>,
        provider: P,
        verifier: BlockVerifier,
        args: ExowareArchiveArgs,
        chain_id: u64,
        genesis_hash: alloy_primitives::B256,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), ArchiveError> {
        let config = args.archive_config(chain_id, genesis_hash)?;
        let source = Arc::new(ProviderSource::new(provider));
        let options = args.publisher_options();
        tokio::pin!(shutdown);
        let mut failures = 0;
        let publisher = loop {
            let opened = tokio::select! {
                // Checked first: a signaled exporter does not provision or open.
                biased;
                () = &mut shutdown => return Ok(()),
                opened = async {
                let archive = ExowareArchive::new(Arc::clone(&store));
                if args.archive_exoware_provision {
                    match archive.provision(&config).await? {
                        ProvisionOutcome::Created => info!(config = %config, "created archive config row"),
                        ProvisionOutcome::AlreadyProvisioned => {
                            info!(config = %config, "archive config row already provisioned")
                        }
                    }
                }
                SegmentPublisher::open(
                    archive,
                    Arc::clone(&source),
                    verifier.clone(),
                    config,
                    options,
                )
                .await
            } => opened,
            };
            match opened {
                Ok(publisher) => break publisher,
                // An unreachable store, or a config row the provisioning node has not written
                // yet (a normal rollout race), is retried; a config mismatch stays terminal.
                Err(err @ (ArchiveError::Store { .. } | ArchiveError::ConfigMissing)) => {
                    failures += 1;
                    warn!(error = %err, failures, "archive store not ready; exporter retries");
                    tokio::select! {
                        () = &mut shutdown => return Ok(()),
                        () = tokio::time::sleep(options.backoff(failures)) => {}
                    }
                }
                Err(err) => return Err(err),
            }
        };
        info!(config = %publisher.config, "archive exporter started");
        publisher.run_until(shutdown).await
    }
}

impl BaseNodeExtension for ExowareArchiveExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let Some(url) = self.args.archive_exoware_url.clone() else { return hooks };
        let args = self.args;
        let export_args = args.clone();
        let export_url = url.clone();
        let hooks = hooks.add_rpc_module(move |ctx| {
            let provider = ctx.provider().clone();
            // Fail fast on retention the archive cannot cover: the provider holds the node's
            // effective prune modes (CLI flags merged with `reth.toml`).
            args.validate_retention(provider.database_provider_ro()?.prune_modes_ref())?;
            let chain_spec = provider.chain_spec();
            let store = Self::store(&args, &url)?;
            info!(
                namespace = %args.archive_exoware_namespace,
                timeout = ?args.archive_exoware_timeout,
                "starting archive-backed eth RPC"
            );
            let archive = Arc::new(ArchiveHandle {
                store,
                node: Arc::new(ProviderSource::new(provider.clone())),
                verifier: BlockVerifier::new(Arc::clone(&chain_spec)),
                chain_id: chain_spec.chain().id(),
                genesis_hash: chain_spec.genesis_hash(),
                limits: ReaderLimits::default(),
                budget: args.archive_exoware_timeout,
                retry_after: RETENTION_TTL,
                reader: OnceCell::new(),
                open_failed_at: Default::default(),
            });
            let rpc = &ctx.config().rpc;
            // Same limits as reth's own filter (`RpcServerArgs::eth_config`).
            let limits = QueryLimits {
                max_blocks_per_filter: Some(rpc.rpc_max_blocks_per_filter.unwrap_or_max()),
                max_logs_per_response: Some(rpc.rpc_max_logs_per_response.unwrap_or_max() as usize),
            };
            let eth_api = ctx.registry.eth_api().clone();
            // The node's own execution and tracing paths (`eth_call`, `eth_callMany`,
            // `trace_*`, `debug_trace*`) read pruned state, blocks and transactions from the
            // archive through this hook; the `debug_` and `trace_` handlers share this API.
            let history = ExowareHistory {
                archive: Arc::clone(&archive),
                provider: provider.clone(),
                converter: ArchiveConverter::new(Arc::clone(&chain_spec)),
                retention: RetentionCache::new(RETENTION_TTL),
                executor: Arc::new(ArchiveExecutor::new()?),
                execution_budget: args.archive_exoware_timeout,
            };
            eth_api.set_pruned_history(Arc::new(history))?;
            // `ArchiveEth` owns routing for the read methods it serves: its local calls use a
            // view without the hook, so each such request reaches the archive at most once.
            let local_eth = eth_api.without_pruned_history();
            let api = ArchiveEth {
                eth: local_eth.clone(),
                filter: ctx.registry.eth_handlers().filter.clone(),
                base_tx: TxBaseEthApiExt::new(local_eth),
                provider,
                archive,
                converter: ArchiveConverter::new(chain_spec),
                limits,
                retention: RetentionCache::new(RETENTION_TTL),
                pool: Arc::new(ctx.pool().clone()),
            };
            ctx.modules.replace_configured(api.into_rpc())?;
            Ok(())
        });
        if !export_args.archive_exoware_export {
            return hooks;
        }
        hooks.add_node_started_hook(move |node| {
            let provider = node.provider.clone();
            let chain_spec = provider.chain_spec();
            let store = Self::store(&export_args, &export_url)?;
            let verifier = BlockVerifier::new(Arc::clone(&chain_spec));
            let (chain_id, genesis_hash) = (chain_spec.chain().id(), chain_spec.genesis_hash());
            node.task_executor.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
                // The guard is held until the exporter returns, so node shutdown waits for
                // the round in flight.
                let (guard_tx, guard_rx) = tokio::sync::oneshot::channel();
                ArchiveMetrics::exporter_running().set(1.0);
                let result = Self::export(
                    store,
                    provider,
                    verifier,
                    export_args,
                    chain_id,
                    genesis_hash,
                    async move {
                        let _ = guard_tx.send(shutdown.await);
                    },
                )
                .await;
                drop(guard_rx);
                // Health signal: the gauge drops to 0 on any exit, and a terminal error is
                // logged at error level, so a halted exporter is visible while the node
                // keeps serving.
                ArchiveMetrics::exporter_running().set(0.0);
                match result {
                    Ok(()) => info!("archive exporter stopped on shutdown"),
                    Err(err) => {
                        error!(error = %err, "archive exporter halted; the node keeps serving")
                    }
                }
            });
            Ok(())
        })
    }
}

impl FromExtensionConfig for ExowareArchiveExtension {
    type Config = ExowareArchiveArgs;

    fn from_config(args: Self::Config) -> Self {
        Self { args }
    }
}
