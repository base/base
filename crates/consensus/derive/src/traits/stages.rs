//! This module contains common traits for stages within the derivation pipeline.

use alloc::boxed::Box;

use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use base_consensus_genesis::SystemConfig;
use base_protocol::BlockInfo;

use crate::{PipelineResult, Signal};

/// Provides typed reset/activation/signal operations for pipeline stages.
///
/// This trait replaces the generic [`SignalReceiver`] for internal stage-to-stage
/// communication, allowing the pipeline core to dispatch typed operations with
/// the correct parameters at each level, rather than threading signal fields
/// (like `l1_origin` and `system_config`) through every signal variant.
#[async_trait]
pub trait StageReset {
    /// Resets the stage to the given L1 origin and system config.
    async fn reset(
        &mut self,
        l1_origin: BlockNumHash,
        system_config: SystemConfig,
    ) -> PipelineResult<()>;

    /// Performs a hardfork activation soft-reset of the stage.
    async fn activate(&mut self) -> PipelineResult<()>;

    /// Flushes the currently active channel.
    async fn flush_channel(&mut self) -> PipelineResult<()>;

    /// Provides a new L1 block to the traversal stage.
    async fn provide_block(&mut self, block: BlockInfo) -> PipelineResult<()>;
}

/// Providers a way for the pipeline to accept a signal from the driver.
#[async_trait]
pub trait SignalReceiver {
    /// Receives a signal from the driver.
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()>;
}

/// Provides a method for accessing the pipeline's current L1 origin.
pub trait OriginProvider {
    /// Returns the optional L1 [`BlockInfo`] origin.
    fn origin(&self) -> Option<BlockInfo>;
}

/// Defines a trait for advancing the L1 origin of the pipeline.
#[async_trait]
pub trait OriginAdvancer {
    /// Advances the internal state of the lowest stage to the next l1 origin.
    /// This method is the equivalent of the reference implementation `advance_l1_block`.
    async fn advance_origin(&mut self) -> PipelineResult<()>;
}
