//! Traits for resetting stages.

use alloc::boxed::Box;

use async_trait::async_trait;
use base_protocol::BlockInfo;
use base_consensus_genesis::SystemConfig;

/// Provides the [`BlockInfo`] and [`SystemConfig`] for the stack to reset the stages.
#[async_trait]
pub trait ResetProvider {
    /// Returns the current [`BlockInfo`] for the pipeline to reset.
    async fn block_info(&self) -> BlockInfo;

    /// Returns the current [`SystemConfig`] for the pipeline to reset.
    async fn system_config(&self) -> SystemConfig;
}
