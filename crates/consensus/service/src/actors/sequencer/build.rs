//! Payload builder for the sequencer.
//!
//! Contains [`PayloadBuilder`], which drives L1 origin selection, attribute
//! preparation, and block build initiation, and [`UnsealedPayloadHandle`],
//! which carries the resulting payload identifier forward to the seal stage.

use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::TIMESTAMP_MILLIS_PER_SECOND;
use base_common_genesis::RollupConfig;
use base_consensus_derive::{AttributesBuilder, PipelineErrorKind};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

use crate::{
    Metrics, PoolActivation,
    actors::{
        SequencerEngineClient,
        sequencer::{
            SequencerTimestamp, SequencerTimestampPlanner,
            error::SequencerActorError,
            origin_selector::{L1OriginSelectorError, OriginSelector},
            recovery::RecoveryModeGuard,
        },
    },
};

/// A block that has been started on the execution layer but not yet sealed.
#[derive(Debug)]
pub struct UnsealedPayloadHandle {
    /// The [`PayloadId`] of the unsealed payload.
    pub payload_id: PayloadId,
    /// The [`AttributesWithParent`] used to start block building.
    pub attributes_with_parent: AttributesWithParent,
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
    /// Cache of the most recently planned child block's full millisecond timestamp.
    ///
    /// Keyed by the child block number. Used to seed [`SequencerTimestampPlanner::beryl_timestamp`]
    /// with an accurate parent millisecond timestamp when the parent itself was produced under
    /// the Subsecond cadence. For the first Subsecond block, or after a cache miss caused by a
    /// restart or reorg, we fall back to `parent.block_info.timestamp * 1000`, which is exact for
    /// pre-Subsecond parents and a safe lower bound otherwise.
    pub last_planned_full_ms: Option<(u64, u64)>,
}

impl<A: AttributesBuilder, O: OriginSelector, E: SequencerEngineClient> PayloadBuilder<A, O, E> {
    /// Starts building the next L2 block, returning a handle to the in-flight payload.
    ///
    /// Uses the engine's current unsafe head (from the watch channel) as the parent.
    /// Returns `Ok(None)` for temporary or reset conditions that should be retried on the
    /// next tick.
    pub async fn build(&mut self) -> Result<Option<UnsealedPayloadHandle>, SequencerActorError> {
        let unsafe_head = self.engine_client.get_unsafe_head().await?;
        self.build_on(unsafe_head).await
    }

    /// Starts building the next L2 block on top of an explicit `parent`, returning a handle to
    /// the in-flight payload.
    ///
    /// Use this when the caller already knows the correct parent, such as after an acknowledged
    /// local insert. That avoids racing the unsafe-head watch channel publication path.
    ///
    /// Returns `Ok(None)` for temporary or reset conditions that should be retried on the
    /// next tick.
    pub async fn build_on(
        &mut self,
        parent: L2BlockInfo,
    ) -> Result<Option<UnsealedPayloadHandle>, SequencerActorError> {
        let Some(l1_origin) = self.get_next_payload_l1_origin(parent).await? else {
            return Ok(None);
        };

        info!(
            target: "sequencer",
            parent_num = parent.block_info.number,
            l1_origin_num = l1_origin.number,
            "Started sequencing new block"
        );

        let attributes_build_start = Instant::now();

        let Some(attributes_with_parent) = self.build_attributes(parent, l1_origin).await? else {
            return Ok(None);
        };

        Metrics::sequencer_attributes_build_duration().record(attributes_build_start.elapsed());

        let build_request_start = Instant::now();

        let payload_id =
            self.engine_client.start_build_block(attributes_with_parent.clone()).await?;

        Metrics::sequencer_block_building_start_task_duration()
            .record(build_request_start.elapsed());

        Ok(Some(UnsealedPayloadHandle { payload_id, attributes_with_parent }))
    }

    /// Determines and validates the L1 origin block for the provided L2 unsafe head.
    ///
    /// Returns `Ok(None)` for temporary errors that should be retried on the next tick.
    pub async fn get_next_payload_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
    ) -> Result<Option<BlockInfo>, SequencerActorError> {
        let l1_origin = match self
            .origin_selector
            .next_l1_origin(unsafe_head, self.recovery_mode.get())
            .await
        {
            Ok(l1_origin) => l1_origin,
            Err(L1OriginSelectorError::OriginNotFound(hash)) => {
                warn!(
                    target: "sequencer",
                    hash = %hash,
                    "L1 origin block not found (reorg or sync lag), triggering engine reset"
                );
                self.engine_client.reset_engine_forkchoice().await?;
                return Ok(None);
            }
            Err(err) => {
                warn!(
                    target: "sequencer",
                    ?err,
                    "Temporary error occurred while selecting next L1 origin. Re-attempting on next tick."
                );
                return Ok(None);
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
            self.engine_client.reset_engine_forkchoice().await?;
            return Ok(None);
        }

        Ok(Some(l1_origin))
    }

    /// Builds the `AttributesWithParent` for the next block.
    ///
    /// Returns `Ok(None)` if no attributes could be built at this time but future
    /// attempts may succeed.
    pub async fn build_attributes(
        &mut self,
        unsafe_head: L2BlockInfo,
        l1_origin: BlockInfo,
    ) -> Result<Option<AttributesWithParent>, SequencerActorError> {
        let planned_timestamp = self.plan_next_timestamp(unsafe_head)?;

        let mut attributes = match self
            .attributes_builder
            .prepare_payload_attributes_at(
                unsafe_head,
                l1_origin.id(),
                planned_timestamp.timestamp,
                planned_timestamp.timestamp_millis_part,
            )
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

        self.rollup_config.log_upgrade_activation(
            unsafe_head.block_info.number.saturating_add(1),
            attributes.payload_attributes.timestamp,
            unsafe_head.block_info.timestamp,
        );
        let activator = PoolActivation::new(Arc::clone(&self.rollup_config));
        attributes.no_tx_pool = Some(!activator.is_enabled(
            self.recovery_mode.get(),
            unsafe_head.block_info.timestamp,
            l1_origin,
            &attributes,
        ));

        let attrs_with_parent = AttributesWithParent::new(attributes, unsafe_head, None, false);
        Ok(Some(attrs_with_parent))
    }

    /// Plans the next L2 block's timestamp.
    ///
    /// After the Subsecond hardfork the sequencer produces blocks every 200ms and the engine
    /// requires a `timestamp_millis_part`. Before Subsecond, we keep the legacy seconds-only
    /// cadence so derivation, batch encoding, and Engine API messaging remain unchanged.
    fn plan_next_timestamp(
        &mut self,
        unsafe_head: L2BlockInfo,
    ) -> Result<SequencerTimestamp, SequencerActorError> {
        let candidate_legacy_ts =
            unsafe_head.block_info.timestamp.saturating_add(self.rollup_config.block_time);
        let use_subsecond = self
            .rollup_config
            .hardforks
            .base
            .subsecond
            .is_some_and(|subsecond_ts| candidate_legacy_ts >= subsecond_ts);

        let planned = if use_subsecond {
            let parent_full_ms = match self.last_planned_full_ms {
                Some((cached_number, cached_ms))
                    if cached_number == unsafe_head.block_info.number =>
                {
                    cached_ms
                }
                _ => unsafe_head
                    .block_info
                    .timestamp
                    .saturating_mul(TIMESTAMP_MILLIS_PER_SECOND as u64),
            };
            let wall_clock_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(parent_full_ms);
            SequencerTimestampPlanner::beryl_timestamp(parent_full_ms, wall_clock_ms)?
        } else {
            SequencerTimestampPlanner::legacy_timestamp(
                unsafe_head.block_info.timestamp,
                self.rollup_config.block_time,
            )?
        };

        self.last_planned_full_ms =
            Some((unsafe_head.block_info.number.saturating_add(1), planned.timestamp_millis));

        Ok(planned)
    }
}
