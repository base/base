//! Reorg actions for the e2e testing framework.

use alloy_primitives::B256;
use base_common_rpc_types_engine::ForkchoiceState;
use eyre::Result;
use futures_util::future::BoxFuture;
use tracing::debug;

use crate::testsuite::{
    BlockInfo, Environment,
    actions::{Action, Sequence, produce_blocks::BroadcastLatestForkchoice},
};

/// Target for reorg operation
#[derive(Debug, Clone)]
pub enum ReorgTarget {
    /// Direct block hash
    Hash(B256),
    /// Tagged block reference
    Tag(String),
}

/// Action that performs a reorg by setting a new head block as canonical
#[derive(Debug)]
pub struct ReorgTo {
    /// Target for the reorg operation
    pub target: ReorgTarget,
}

impl ReorgTo {
    /// Create a new `ReorgTo` action with a direct block hash
    pub const fn new(target_hash: B256) -> Self {
        Self { target: ReorgTarget::Hash(target_hash) }
    }

    /// Create a new `ReorgTo` action with a tagged block reference
    pub fn new_from_tag(tag: impl Into<String>) -> Self {
        Self { target: ReorgTarget::Tag(tag.into()) }
    }
}

impl Action for ReorgTo {
    fn execute<'a>(&'a mut self, env: &'a mut Environment) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // resolve the target block info from either direct hash or tag
            let target_block_info = match &self.target {
                ReorgTarget::Hash(_hash) => {
                    return Err(eyre::eyre!(
                        "Direct hash reorgs are not supported. Use CaptureBlock to tag the target block first, then use ReorgTo::new_from_tag()"
                    ));
                }
                ReorgTarget::Tag(tag) => {
                    let (block_info, _node_idx) =
                        env.block_registry.get(tag).copied().ok_or_else(|| {
                            eyre::eyre!("Block tag '{}' not found in registry", tag)
                        })?;
                    block_info
                }
            };

            let mut sequence = Sequence::new(vec![
                Box::new(SetReorgTarget::new(target_block_info)),
                Box::new(BroadcastLatestForkchoice::default()),
            ]);

            sequence.execute(env).await
        })
    }
}

/// Sub-action to set the reorg target block in the environment
#[derive(Debug)]
pub struct SetReorgTarget {
    /// Complete block info for the reorg target
    pub target_block_info: BlockInfo,
}

impl SetReorgTarget {
    /// Create a new `SetReorgTarget` action
    pub const fn new(target_block_info: BlockInfo) -> Self {
        Self { target_block_info }
    }
}

impl Action for SetReorgTarget {
    fn execute<'a>(&'a mut self, env: &'a mut Environment) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let block_info = self.target_block_info;

            debug!(
                "Setting reorg target to block {} (hash: {})",
                block_info.number, block_info.hash
            );

            // update active node state to point to the target block
            let active_node_state = env.active_node_state_mut()?;
            active_node_state.current_block_info = Some(block_info);
            active_node_state.latest_header_time = block_info.timestamp;

            // update fork choice state to make the target block canonical
            active_node_state.latest_fork_choice_state = ForkchoiceState {
                head_block_hash: block_info.hash,
                safe_block_hash: block_info.hash,
                finalized_block_hash: block_info.hash,
            };

            debug!("Set reorg target to block {}", block_info.hash);
            Ok(())
        })
    }
}
