//! Wires the `eth_sendBundle` RPC and bundle transaction maintenance task.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use base_execution_txpool::{SendBundleApiImpl, SendBundleApiServer, maintain_bundle_transactions};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use reth_chain_state::CanonStateSubscriptions;
use reth_provider::BlockNumReader;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;

/// Extension that enables `eth_sendBundle` RPC support and bundle lifecycle management.
#[derive(Debug)]
pub struct BundleExtension;

impl FromExtensionConfig for BundleExtension {
    type Config = ();

    fn from_config(_config: Self::Config) -> Self {
        Self
    }
}

impl BaseNodeExtension for BundleExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let current_block_number = Arc::new(AtomicU64::new(0));
        let block_number_for_rpc = Arc::clone(&current_block_number);

        let hooks = hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = SendBundleApiImpl::new(ctx.pool().clone(), true, block_number_for_rpc);
            ctx.modules.merge_configured(api.into_rpc())?;
            info!("eth_sendBundle RPC enabled");
            Ok(())
        });

        hooks.add_node_started_hook(move |ctx| {
            let latest = ctx.provider().best_block_number().unwrap_or(0);
            current_block_number.store(latest, Ordering::Release);

            let pool = ctx.pool().clone();
            let events = BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());

            ctx.task_executor.spawn_critical_task(
                "bundle-maintenance",
                maintain_bundle_transactions(pool, events, current_block_number),
            );
            info!("Bundle maintenance task spawned");
            Ok(())
        })
    }
}
