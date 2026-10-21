//! A task to consolidate the engine state.

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_protocol::{AttributesWithParent, L2BlockInfo};

use crate::{
    AttributesMatch, ConsolidateTaskError, EngineClient, EngineState, EngineTaskExt,
    InsertPayloadSafety, SynchronizeTask, state::EngineSyncStateUpdate, task_queue::build_and_seal,
};

/// The [`ConsolidateTask`] attempts to consolidate the engine state using derived payload
/// attributes.
#[derive(Debug, Clone)]
pub struct ConsolidateTask<EngineClient_: EngineClient> {
    /// The engine client.
    pub client: Arc<EngineClient_>,
    /// The [`RollupConfig`].
    pub cfg: Arc<RollupConfig>,
    /// The derived attributes to consolidate.
    pub attributes: AttributesWithParent,
}

impl<EngineClient_: EngineClient> ConsolidateTask<EngineClient_> {
    /// Creates a new [`ConsolidateTask`] for the derived attributes.
    pub const fn new(
        client: Arc<EngineClient_>,
        cfg: Arc<RollupConfig>,
        attributes: AttributesWithParent,
    ) -> Self {
        Self { client, cfg, attributes }
    }

    /// Builds and seals the block of the derived attributes as safe.
    async fn build_and_seal_safe(
        &self,
        state: &mut EngineState,
    ) -> Result<(), ConsolidateTaskError> {
        build_and_seal(
            state,
            Arc::clone(&self.client),
            Arc::clone(&self.cfg),
            self.attributes.clone(),
            InsertPayloadSafety::Safe,
        )
        .await?;

        Ok(())
    }

    /// If safe derivation is about to build on the current safe head, first check whether the EL
    /// already has the derived block. This covers the case where an earlier unsafe FCU returned
    /// `SYNCING`, reth later backfilled the block, but consensus never advanced its unsafe head.
    async fn reconcile_existing_derived_block(
        &self,
        state: &mut EngineState,
    ) -> Result<bool, ConsolidateTaskError> {
        let block_num = self.attributes.block_number();
        let fetch_start = Instant::now();
        let block = match self.client.l2_block_by_label(block_num.into()).await {
            Ok(Some(block)) => block,
            Ok(None) => {
                debug!(
                    target: "engine",
                    block_num,
                    "Derived block not found in EL; proceeding to build fallback"
                );
                return Ok(false);
            }
            Err(err) => {
                warn!(
                    target: "engine",
                    block_num,
                    error = ?err,
                    "Failed to fetch derived L2 block before build fallback"
                );
                return Err(ConsolidateTaskError::FailedToFetchDerivedL2Block);
            }
        };
        let block_fetch_duration = fetch_start.elapsed();
        let block_hash = block.header.hash;
        let block = block.map_header(|header| header.into_inner());

        if !AttributesMatch::check(&self.cfg, &self.attributes, &block).is_match() {
            debug!(
                target: "engine",
                attributes = ?self.attributes,
                block_hash = %block_hash,
                "Derived block does not match attributes; proceeding to build fallback",
            );
            return Ok(false);
        }

        let block_info = match L2BlockInfo::from_block_and_genesis(
            &block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
            &self.cfg.genesis,
        ) {
            Ok(block_info) => block_info,
            Err(e) => {
                warn!(target: "engine", error = ?e, "Failed to construct L2BlockInfo from derived block; proceeding to build fallback");
                return Ok(false);
            }
        };

        // The unsafe head lags the derived block here, so it advances with the safe heads.
        SynchronizeTask::new(
            Arc::clone(&self.client),
            Arc::clone(&self.cfg),
            EngineSyncStateUpdate {
                unsafe_head: Some(block_info),
                local_safe_head: Some(block_info),
                safe_head: Some(block_info),
                ..Default::default()
            },
        )
        .execute(state)
        .await
        .map_err(|e| {
            warn!(target: "engine", error = ?e, "Apply safe head failed");
            e
        })?;

        if state.sync_state.unsafe_head() != block_info
            || state.sync_state.local_safe_head() != block_info
            || state.sync_state.safe_head() != block_info
        {
            warn!(
                target: "engine",
                safe_l2 = %block_info,
                unsafe_head = %state.sync_state.unsafe_head(),
                local_safe_head = %state.sync_state.local_safe_head(),
                safe_head = %state.sync_state.safe_head(),
                "Apply safe head did not advance engine state"
            );
            return Err(ConsolidateTaskError::ForkchoiceUpdateDidNotApply);
        }

        info!(
            target: "engine",
            hash = %block_info.block_info.hash,
            number = block_info.block_info.number,
            ?block_fetch_duration,
            "Reconciled existing derived block before build fallback"
        );

        Ok(true)
    }

    /// Attempts consolidation on the engine state.
    pub async fn consolidate(&self, state: &mut EngineState) -> Result<(), ConsolidateTaskError> {
        let global_start = Instant::now();

        // Fetch the unsafe L2 block
        let block_num = self.attributes.block_number();
        let fetch_start = Instant::now();
        let block = match self.client.l2_block_by_label(block_num.into()).await {
            Ok(Some(block)) => block,
            Ok(None) => {
                warn!(target: "engine", block_num, "Received `None` block");
                return Err(ConsolidateTaskError::MissingUnsafeL2Block(block_num));
            }
            Err(_) => {
                warn!(target: "engine", "Failed to fetch unsafe l2 block for consolidation");
                return Err(ConsolidateTaskError::FailedToFetchUnsafeL2Block);
            }
        };
        let block_fetch_duration = fetch_start.elapsed();
        let block_hash = block.header.hash;
        let block = block.map_header(|header| header.into_inner());

        if AttributesMatch::check(&self.cfg, &self.attributes, &block).is_match() {
            trace!(
                target: "engine",
                attributes = ?self.attributes,
                block_hash = %block_hash,
                "Consolidating engine state",
            );
            match L2BlockInfo::from_block_and_genesis(
                &block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
                &self.cfg.genesis,
            ) {
                // Only issue a forkchoice update if the attributes are the last in the span
                // batch. This is an optimization to avoid sending a FCU
                // call for every block in the span batch.
                Ok(block_info) if !self.attributes.is_last_in_span => {
                    let total_duration = global_start.elapsed();

                    // Apply a transient update to the safe head.
                    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
                        local_safe_head: Some(block_info),
                        safe_head: Some(block_info),
                        ..Default::default()
                    });

                    info!(
                        target: "engine",
                        hash = %block_info.block_info.hash,
                        number = block_info.block_info.number,
                        ?total_duration,
                        ?block_fetch_duration,
                        "Updated safe head via L1 consolidation"
                    );

                    return Ok(());
                }
                Ok(block_info) => {
                    let fcu_start = Instant::now();

                    SynchronizeTask::new(
                        Arc::clone(&self.client),
                        Arc::clone(&self.cfg),
                        EngineSyncStateUpdate {
                            local_safe_head: Some(block_info),
                            safe_head: Some(block_info),
                            ..Default::default()
                        },
                    )
                    .execute(state)
                    .await
                    .map_err(|e| {
                        warn!(target: "engine", error = ?e, "Consolidation failed");
                        e
                    })?;

                    let fcu_duration = fcu_start.elapsed();
                    let total_duration = global_start.elapsed();

                    info!(
                        target: "engine",
                        hash = %block_info.block_info.hash,
                        number = block_info.block_info.number,
                        ?total_duration,
                        ?block_fetch_duration,
                        fcu_duration = ?fcu_duration,
                        "Updated safe head via L1 consolidation"
                    );

                    return Ok(());
                }
                Err(e) => {
                    // Continue on to build the block since we failed to construct the block info.
                    warn!(target: "engine", error = ?e, "Failed to construct L2BlockInfo, proceeding to build task");
                }
            }
        }

        debug!(
            target: "engine",
            attributes = ?self.attributes,
            block_hash = %block_hash,
            "Derived attributes do not match the unsafe block; initiating reorg",
        );
        self.build_and_seal_safe(state).await
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for ConsolidateTask<EngineClient_> {
    type Output = ();

    type Error = ConsolidateTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<(), ConsolidateTaskError> {
        if state.sync_state.safe_head().block_info.number
            < state.sync_state.unsafe_head().block_info.number
        {
            self.consolidate(state).await
        } else if self.reconcile_existing_derived_block(state).await? {
            Ok(())
        } else {
            self.build_and_seal_safe(state).await
        }
    }
}
