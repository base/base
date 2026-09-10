use alloy_eips::BlockNumHash;
use base_common_runtime_tasks::TaskExecutor;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_network_service::NetworkHandle;
use base_execution_state_provider::providers::BlockchainProvider;
use tokio::sync::mpsc::UnboundedSender;

use crate::{ExExEvent, ExExNotifications};

/// Handles used by the node's proof-history and indexing observers.
#[derive(Debug)]
pub struct ExExContext {
    /// Canonical head when the observer started.
    pub head: BlockNumHash,
    /// Observer progress events used to coordinate pruning.
    pub events: UnboundedSender<ExExEvent>,
    /// Canonical-chain notifications and backfill stream.
    pub notifications: ExExNotifications<BlockchainProvider>,
    /// Read access to canonical blocks and state.
    pub provider: BlockchainProvider,
    /// Execution rules used when backfilling history.
    pub evm_config: BaseEvmConfig,
    /// Executor for observer background work.
    pub task_executor: TaskExecutor,
    /// Network synchronization status for the indexer.
    pub network: NetworkHandle,
}
