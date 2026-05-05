//! Utility functions for direct engine operations.

use std::sync::Arc;

use base_common_genesis::RollupConfig;
use base_protocol::AttributesWithParent;

use super::{BuildTaskError, SealTask, SealTaskError};
use crate::{Engine, EngineClient, EngineState, InsertPayloadSafety};

/// Error type for build and seal operations.
#[derive(Debug, thiserror::Error)]
pub(in crate::task_queue) enum BuildAndSealError {
    /// An error occurred during the build phase.
    #[error(transparent)]
    Build(#[from] BuildTaskError),
    /// An error occurred during the seal phase.
    #[error(transparent)]
    Seal(#[from] SealTaskError),
}

/// Builds and seals a payload in sequence.
///
/// This is a utility function that:
/// 1. Starts an execution-layer build
/// 2. Seals the block, referencing the initiated payload
///
/// This pattern is commonly used for Holocene deposits-only fallback and other scenarios
/// where a build-then-seal workflow is needed.
///
/// # Arguments
///
/// * `state` - Mutable reference to the engine state
/// * `engine` - The engine client
/// * `cfg` - The rollup configuration
/// * `attributes` - The payload attributes to build
/// * `payload_safety` - Whether the sealed payload should advance the safe head
pub(in crate::task_queue) async fn build_and_seal<EngineClient_: EngineClient>(
    state: &mut EngineState,
    engine: Arc<EngineClient_>,
    cfg: Arc<RollupConfig>,
    attributes: AttributesWithParent,
    payload_safety: InsertPayloadSafety,
) -> Result<(), BuildAndSealError> {
    let payload_id =
        Engine::build_with_state(state, engine.as_ref(), cfg.as_ref(), attributes.clone()).await?;

    SealTask::new(engine, cfg, payload_id, attributes, payload_safety, None).execute(state).await?;

    Ok(())
}
