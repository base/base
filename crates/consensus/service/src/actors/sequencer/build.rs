//! Payload builder for the sequencer.
//!
//! Contains [`PayloadBuilder`], which drives L1 origin selection, attribute
//! preparation, and block build initiation, [`BuildOutcome`], which preserves
//! why a build could not start, and [`UnsealedPayloadHandle`], which carries the
//! resulting payload identifier forward to the seal stage.

use std::{sync::Arc, time::Instant};

use alloy_rpc_types_engine::PayloadId;
use base_common_genesis::RollupConfig;
use base_consensus_derive::{AttributesBuilder, PipelineErrorKind};
use base_consensus_engine::{BuildTaskError, EngineBuildError};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
use tracing::instrument;

use crate::{
    EngineClientError, Metrics, PoolActivation, ResetReason,
    actors::{
        SequencerEngineClient,
        sequencer::{
            error::SequencerActorError,
            l1_origin::{L1OriginSelectorError, OriginSelector},
            recovery::RecoveryModeGuard,
            shadow_funding::ShadowFunding,
        },
    },
};

/// The outcome of a build step that may produce a value of type `T`.
#[derive(Debug)]
pub enum BuildOutcome<T> {
    /// The build step produced its value.
    Ready(T),
    /// The build was deferred by a temporary condition unrelated to sequencer drift.
    Deferred,
    /// Sequencer drift requires advancing, but the next L1 origin is not ready yet.
    AwaitingL1Origin,
}

/// A block that has been started on the execution layer but not yet sealed.
#[derive(Debug)]
pub struct UnsealedPayloadHandle {
    /// The [`PayloadId`] of the unsealed payload.
    pub payload_id: PayloadId,
    /// The [`AttributesWithParent`] used to start block building.
    pub attributes_with_parent: AttributesWithParent,
}

impl UnsealedPayloadHandle {
    /// Returns the number of the block represented by this payload.
    pub const fn block_number(&self) -> u64 {
        self.attributes_with_parent.parent().block_info.number.saturating_add(1)
    }
}

/// Drives payload attribute preparation and block build initiation.
///
/// Owns the build-side dependencies (`attributes_builder`, `origin_selector`,
/// `engine_client`) so the sequencer actor can delegate the full build phase
/// with a single [`PayloadBuilder::build`] call, without threading those
/// resources through as parameters on every tick.
#[derive(Debug)]
pub struct PayloadBuilder<A: AttributesBuilder, O: OriginSelector, E: SequencerEngineClient> {
    /// The attributes builder.
    pub attributes_builder: A,
    /// The engine client.
    pub engine_client: Arc<E>,
    /// The origin selector.
    pub origin_selector: O,
    /// Shared recovery mode flag.
    pub recovery_mode: RecoveryModeGuard,
    /// The rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
}

impl<A: AttributesBuilder, O: OriginSelector, E: SequencerEngineClient> PayloadBuilder<A, O, E> {
    /// Starts building the next L2 block, returning a handle to the in-flight payload.
    ///
    /// Uses the engine's current unsafe head (from the watch channel) as the parent.
    pub async fn build(
        &mut self,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<BuildOutcome<UnsealedPayloadHandle>, SequencerActorError> {
        let unsafe_head = self.engine_client.get_unsafe_head().await?;
        self.build_on(unsafe_head, shadow_funding).await
    }

    /// Starts building the next L2 block on top of an explicit `parent`, returning a handle to
    /// the in-flight payload.
    ///
    /// Use this when the caller already knows the correct parent, such as after an acknowledged
    /// local insert. That avoids racing the unsafe-head watch channel publication path.
    #[instrument(skip_all, fields(parent_num = parent.block_info.number, l1_origin_num = tracing::field::Empty))]
    pub async fn build_on(
        &mut self,
        parent: L2BlockInfo,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<BuildOutcome<UnsealedPayloadHandle>, SequencerActorError> {
        let l1_origin = match self.get_next_payload_l1_origin(parent).await? {
            BuildOutcome::Ready(l1_origin) => l1_origin,
            BuildOutcome::Deferred => return Ok(BuildOutcome::Deferred),
            BuildOutcome::AwaitingL1Origin => return Ok(BuildOutcome::AwaitingL1Origin),
        };
        tracing::Span::current().record("l1_origin_num", l1_origin.number);

        info!(
            target: "sequencer",
            parent_num = parent.block_info.number,
            l1_origin_num = l1_origin.number,
            "Started sequencing new block"
        );

        let attributes_build_start = Instant::now();

        let Some(attributes_with_parent) =
            self.build_attributes(parent, l1_origin, shadow_funding).await?
        else {
            return Ok(BuildOutcome::Deferred);
        };

        Metrics::sequencer_attributes_build_duration().record(attributes_build_start.elapsed());

        let build_request_start = Instant::now();

        let payload_id =
            match self.engine_client.start_build_block(attributes_with_parent.clone()).await {
                Ok(payload_id) => payload_id,
                Err(EngineClientError::StartBuildError(BuildTaskError::EngineBuildError(
                    EngineBuildError::EngineSyncing,
                ))) => {
                    warn!(target: "sequencer", "EL sync in progress; deferring payload build");
                    return Ok(BuildOutcome::Deferred);
                }
                Err(err) => return Err(err.into()),
            };

        Metrics::sequencer_block_building_start_task_duration()
            .record(build_request_start.elapsed());

        Ok(BuildOutcome::Ready(UnsealedPayloadHandle { payload_id, attributes_with_parent }))
    }

    /// Determines and validates the L1 origin block for the provided L2 unsafe head.
    pub async fn get_next_payload_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
    ) -> Result<BuildOutcome<BlockInfo>, SequencerActorError> {
        let l1_origin = match self.origin_selector.next_l1_origin(unsafe_head).await {
            Ok(l1_origin) => l1_origin,
            Err(L1OriginSelectorError::OriginNotFound(hash)) => {
                warn!(
                    target: "sequencer",
                    hash = %hash,
                    "L1 origin block not found (reorg or sync lag), triggering engine reset"
                );
                self.engine_client
                    .reset_engine_forkchoice(ResetReason::L1OriginUnavailable)
                    .await?;
                return Ok(BuildOutcome::Deferred);
            }
            Err(err @ L1OriginSelectorError::NextL1OriginOrphaned { .. }) => {
                warn!(
                    target: "sequencer",
                    ?err,
                    "Next L1 origin orphaned the accepted current origin, triggering engine reset"
                );
                self.engine_client.reset_engine_forkchoice(ResetReason::L1OriginOrphaned).await?;
                return Ok(BuildOutcome::Deferred);
            }
            Err(err @ L1OriginSelectorError::NotEnoughData(_)) => {
                warn!(
                    target: "sequencer",
                    ?err,
                    "Next L1 origin is not ready after sequencer drift; deferring block build"
                );
                return Ok(BuildOutcome::AwaitingL1Origin);
            }
            Err(err) => {
                warn!(
                    target: "sequencer",
                    ?err,
                    "Temporary error occurred while selecting next L1 origin. Re-attempting on next tick."
                );
                return Ok(BuildOutcome::Deferred);
            }
        };

        if unsafe_head.l1_origin.hash != l1_origin.parent_hash
            && unsafe_head.l1_origin.hash != l1_origin.hash
        {
            warn!(
                target: "sequencer",
                l1_origin = ?l1_origin,
                unsafe_head_hash = %unsafe_head.l1_origin.hash,
                unsafe_head_l1_origin = ?unsafe_head.l1_origin,
                "Cannot build new L2 block on inconsistent L1 origin, resetting engine"
            );
            self.engine_client.reset_engine_forkchoice(ResetReason::L1OriginInconsistent).await?;
            return Ok(BuildOutcome::Deferred);
        }

        Ok(BuildOutcome::Ready(l1_origin))
    }

    /// Builds the `AttributesWithParent` for the next block.
    ///
    /// Returns `Ok(None)` if no attributes could be built at this time but future
    /// attempts may succeed.
    pub async fn build_attributes(
        &mut self,
        unsafe_head: L2BlockInfo,
        l1_origin: BlockInfo,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<Option<AttributesWithParent>, SequencerActorError> {
        let mut attributes = match self
            .attributes_builder
            .prepare_payload_attributes(unsafe_head, l1_origin.id())
            .await
        {
            Ok(attrs) => attrs,
            Err(PipelineErrorKind::Temporary(_)) => return Ok(None),
            Err(PipelineErrorKind::Reset(err)) => {
                // The attributes builder returned a reset error. These errors fall into two
                // categories, neither of which requires an engine reset here:
                //
                // 1. L1 origin inconsistency (BlockMismatch / BlockMismatchEpochReset):
                //    `get_next_payload_l1_origin` already validates L1 origin consistency and
                //    calls `reset_engine_forkchoice` if it detects a mismatch. If execution
                //    reaches `build_attributes`, the L1 origin passed in was already validated.
                //    Any residual mismatch is a transient provider race that resolves on retry.
                //
                // 2. BrokenTimeInvariant: the next L2 timestamp would precede the selected L1
                //    block's timestamp. This is a timing condition — the origin selector will
                //    pick a different L1 block on the next tick. Engine reset would rewind the
                //    unsafe head to the safe head, discarding sequenced progress unnecessarily.
                //
                // Return Ok(None) and let the ticker retry on the next block interval.
                warn!(
                    target: "sequencer",
                    error = ?err,
                    "Pipeline reset error while preparing payload attributes, retrying on next tick"
                );
                return Ok(None);
            }
            Err(err @ PipelineErrorKind::Critical(_)) => {
                error!(target: "sequencer", ?err, "Failed to prepare payload attributes");
                return Err(err.into());
            }
        };

        if let Some(funding) = shadow_funding {
            attributes
                .transactions
                .get_or_insert_default()
                .push(funding.transaction(unsafe_head.block_info.hash));
        }

        self.rollup_config.log_upgrade_activation(
            unsafe_head.block_info.number.saturating_add(1),
            attributes.payload_attributes.timestamp,
            unsafe_head.block_info.timestamp,
        );
        let activator = PoolActivation::new(Arc::clone(&self.rollup_config));
        attributes.no_tx_pool = Some(!activator.is_enabled(
            self.recovery_mode.get(),
            l1_origin,
            unsafe_head.block_info.timestamp,
            &attributes,
        ));

        let attrs_with_parent = AttributesWithParent::new(attributes, unsafe_head, None, false);
        Ok(Some(attrs_with_parent))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_consensus_derive::test_utils::TestAttributesBuilder;
    use base_consensus_engine::{BuildTaskError, EngineBuildError};
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::{BuildOutcome, PayloadBuilder};
    use crate::{
        EngineClientError, MockOriginSelector, MockSequencerEngineClient,
        actors::sequencer::RecoveryModeGuard,
    };

    #[tokio::test]
    async fn engine_syncing_defers_build() {
        let mut origin_selector = MockOriginSelector::new();
        origin_selector.expect_next_l1_origin().once().return_once(|_| Ok(BlockInfo::default()));

        let mut engine_client = MockSequencerEngineClient::new();
        engine_client.expect_start_build_block().once().return_once(|_| {
            Err(EngineClientError::StartBuildError(BuildTaskError::EngineBuildError(
                EngineBuildError::EngineSyncing,
            )))
        });

        let mut builder = PayloadBuilder {
            attributes_builder: TestAttributesBuilder {
                attributes: vec![Ok(BasePayloadAttributes::default())],
                ..Default::default()
            },
            engine_client: Arc::new(engine_client),
            origin_selector,
            recovery_mode: RecoveryModeGuard::new(false),
            rollup_config: Arc::new(RollupConfig::default()),
        };

        let outcome = builder.build_on(L2BlockInfo::default(), None).await.unwrap();

        assert!(matches!(outcome, BuildOutcome::Deferred));
    }
}
