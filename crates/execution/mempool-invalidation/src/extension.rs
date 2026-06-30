//! Wires the canonical-state-driven mempool invalidation maintenance task.

use base_execution_txpool::maintain_state_diff_invalidation;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_chain_state::CanonStateSubscriptions;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;

/// Extension that feeds committed-block state diffs into the transaction pool's
/// exact-match invalidation index, dropping now-invalid channelized EIP-8130
/// transactions ahead of the builder.
#[derive(Debug)]
pub struct MempoolInvalidationExtension;

impl FromExtensionConfig for MempoolInvalidationExtension {
    type Config = ();

    fn from_config(_config: Self::Config) -> Self {
        Self
    }
}

impl BaseNodeExtension for MempoolInvalidationExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_node_started_hook(move |ctx| {
            let pool = ctx.pool().clone();
            let events = BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());

            ctx.task_executor.spawn_critical_task(
                "mempool-invalidation",
                maintain_state_diff_invalidation(pool, events),
            );
            info!("Mempool state-diff invalidation task spawned");
            Ok(())
        })
    }
}
