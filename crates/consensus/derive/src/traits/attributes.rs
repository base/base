//! Contains traits for working with payload attributes and their providers.

use alloc::boxed::Box;
use core::fmt::Debug;

use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_protocol::{AttributesWithParent, L2BlockInfo, SingleBatch};

use crate::PipelineResult;

/// [`AttributesProvider`] is a trait abstraction that generalizes the [`BatchQueue`] stage.
///
/// [`BatchQueue`]: crate::stages::BatchQueue
#[async_trait]
pub trait AttributesProvider {
    /// Returns the next valid batch upon the given safe head.
    async fn next_batch(&mut self, parent: L2BlockInfo) -> PipelineResult<SingleBatch>;

    /// Returns whether the current batch is the last in its span.
    fn is_last_in_span(&self) -> bool;
}

/// [`NextAttributes`] defines the interface for pulling attributes from
/// the top level `AttributesQueue` stage of the pipeline.
#[async_trait]
pub trait NextAttributes {
    /// Returns the next [`AttributesWithParent`] from the current batch.
    async fn next_attributes(
        &mut self,
        parent: L2BlockInfo,
    ) -> PipelineResult<AttributesWithParent>;
}

/// The [`AttributesBuilder`] is responsible for preparing [`BasePayloadAttributes`]
/// that can be used to construct an L2 Block containing only deposits.
#[async_trait]
pub trait AttributesBuilder: Debug + Send {
    /// Prepares a template [`BasePayloadAttributes`] that is ready to be used to build an L2
    /// block. The block will contain deposits only, on top of the given L2 parent, with the L1
    /// origin set to the given epoch.
    /// By default, the [`BasePayloadAttributes`] template will have `no_tx_pool` set to true,
    /// and no sequencer transactions. The caller has to modify the template to add transactions.
    /// This can be done by either setting the `no_tx_pool` to false as sequencer, or by appending
    /// batch transactions as the verifier.
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<BasePayloadAttributes>;

    /// Prepares a template [`BasePayloadAttributes`] for an explicitly chosen L2 timestamp.
    ///
    /// Sequencer-side timestamp production can choose a sub-second Base extension while keeping
    /// `payload_attributes.timestamp` seconds-denominated. Implementations must use the provided
    /// timestamp consistently for the payload attributes and the encoded L1 info transaction.
    async fn prepare_payload_attributes_at(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: BlockNumHash,
        l2_timestamp: u64,
        timestamp_millis_part: Option<u16>,
    ) -> PipelineResult<BasePayloadAttributes>;
}
