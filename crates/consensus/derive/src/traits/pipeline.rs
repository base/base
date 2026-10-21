//! Defines the interface for the core derivation pipeline.

use alloc::boxed::Box;
use core::iter::Iterator;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_protocol::{AttributesWithParent, L2BlockInfo};

use crate::{OriginProvider, PipelineErrorKind, StepResult};

/// This trait defines the interface for interacting with the derivation pipeline.
#[async_trait]
pub trait Pipeline: OriginProvider + Iterator<Item = AttributesWithParent> {
    /// Peeks at the next [`AttributesWithParent`] from the pipeline.
    fn peek(&self) -> Option<&AttributesWithParent>;

    /// Attempts to progress the pipeline.
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult;

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig;

    /// Returns the [`SystemConfig`] for the L2 block with the given hash.
    async fn system_config_by_l2_hash(
        &mut self,
        hash: B256,
    ) -> Result<SystemConfig, PipelineErrorKind>;
}
