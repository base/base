use std::{
    future::Future,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Block;
use alloy_rpc_types_engine::{
    ExecutionPayloadFieldV2, ExecutionPayloadInputV2, ForkchoiceState, ForkchoiceUpdated,
    PayloadStatus, PayloadStatusEnum,
};
use alloy_transport::{TransportError, TransportResult};
use base_clock::GlobalClock;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseTxEnvelope, EIP1559ParamError};
use base_common_network::BaseEngineApi;
use base_common_rpc_types_engine::BaseExecutionPayload;

use crate::{Candidate, CanonicalAttributes};

/// Scheduling policy before requesting the final payload.
#[derive(Debug, Default, Clone, Copy)]
pub enum BuildWait {
    /// Wait until attributes.timestamp according to the process-wide replay clock.
    #[default]
    UntilTimestamp,
    /// Allow a fixed build interval; this cannot repair an already-expired builder deadline.
    For(Duration),
}

/// Bounded Engine API driver. Repeated calls always build on the supplied canonical parent.
#[derive(Debug)]
pub struct ShadowDriver {
    /// Maximum duration of each RPC/status-retry phase and the build wait.
    pub timeout: Duration,
    /// Build scheduling policy shared with the builder's replay clock.
    pub wait: BuildWait,
    /// Optional replay-clock deadline checked before each attributes-bearing FCU attempt.
    pub build_deadline: Option<SystemTime>,
}

impl Default for ShadowDriver {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(15), wait: BuildWait::default(), build_deadline: None }
    }
}

/// Terminal replay failure; transient ACCEPTED/SYNCING statuses are retried until timeout.
#[derive(Debug, thiserror::Error)]
pub enum ShadowError {
    /// The build budget expired before an attributes-bearing FCU attempt.
    #[error("builder deadline missed for block {0}")]
    MissedDeadline(u64),
    /// RPC or retry phase exceeded its configured bound.
    #[error("shadow phase timed out: {0}")]
    Timeout(&'static str),
    /// Transport/protocol errors are terminal rather than blindly retried.
    #[error(transparent)]
    Rpc(#[from] TransportError),
    /// Canonical attributes could not be decoded.
    #[error(transparent)]
    Attributes(#[from] EIP1559ParamError),
    /// Engine rejected the block, retaining validation details for structured reporting.
    #[error("shadow engine rejected block {block_number}: {status:?}")]
    Invalid {
        /// Canonical block being replayed.
        block_number: u64,
        /// Engine status, latest valid hash, and validation error.
        status: Box<PayloadStatus>,
    },
    /// A valid build request did not produce a build identifier.
    #[error("valid forkchoice response omitted payload ID")]
    MissingPayloadId,
    /// Wall clock predates the Unix epoch.
    #[error(transparent)]
    Clock(#[from] std::time::SystemTimeError),
}

impl ShadowDriver {
    /// Builds and records a candidate, discards it, then imports and selects the canonical block.
    ///
    /// # Errors
    /// Returns a terminal engine, attribute, clock, or timeout error. No subsequent block is driven.
    ///
    /// # Cancel safety
    /// Cancellation or timeout can leave an in-flight build/import on the engine. Resume only from
    /// a known canonical head; this method never imports the candidate or spawns detached tasks.
    pub async fn replay_block(
        &self,
        engine: &(impl BaseEngineApi + Sync),
        block: &Block<BaseTxEnvelope>,
        chain: &impl Upgrades,
    ) -> Result<Candidate, ShadowError> {
        let attrs = CanonicalAttributes::derive(block, chain)?;
        let timestamp = block.header.timestamp;
        let ecotone = chain.is_ecotone_active_at_timestamp(timestamp);
        let forkchoice = |state, attrs| async move {
            if ecotone {
                engine.fork_choice_updated_v3(state, attrs).await
            } else {
                engine.fork_choice_updated_v2(state, attrs).await
            }
        };
        let parent = ForkchoiceState {
            head_block_hash: block.header.parent_hash,
            safe_block_hash: block.header.parent_hash,
            finalized_block_hash: block.header.parent_hash,
        };
        let start = Instant::now();
        let fcu = self
            .valid(block.header.number, self.build_deadline, || {
                forkchoice(parent, Some(attrs.clone()))
            })
            .await?;
        let id = fcu.payload_id.ok_or(ShadowError::MissingPayloadId)?;
        let wait = match self.wait {
            BuildWait::UntilTimestamp => Duration::from_secs(timestamp)
                .saturating_sub(GlobalClock::get().now().duration_since(UNIX_EPOCH)?),
            BuildWait::For(duration) => duration,
        };
        tokio::time::timeout(self.timeout, tokio::time::sleep(wait))
            .await
            .map_err(|_| ShadowError::Timeout("build wait"))?;
        let (payload, block_value) = tokio::time::timeout(self.timeout, async {
            Ok::<_, TransportError>(if chain.is_azul_active_at_timestamp(timestamp) {
                let envelope = engine.get_payload_v5(id).await?;
                (BaseExecutionPayload::V4(envelope.execution_payload), envelope.block_value)
            } else if chain.is_isthmus_active_at_timestamp(timestamp) {
                let envelope = engine.get_payload_v4(id).await?;
                (BaseExecutionPayload::V4(envelope.execution_payload), envelope.block_value)
            } else if chain.is_ecotone_active_at_timestamp(timestamp) {
                let envelope = engine.get_payload_v3(id).await?;
                (BaseExecutionPayload::V3(envelope.execution_payload), envelope.block_value)
            } else {
                let envelope = engine.get_payload_v2(id).await?;
                let payload = match envelope.execution_payload {
                    ExecutionPayloadFieldV2::V1(payload) => BaseExecutionPayload::V1(payload),
                    ExecutionPayloadFieldV2::V2(payload) => BaseExecutionPayload::V2(payload),
                };
                (payload, envelope.block_value)
            })
        })
        .await
        .map_err(|_| ShadowError::Timeout("getPayload"))??;
        let candidate = Candidate::from_payload(payload.as_v1(), block_value, start.elapsed());
        drop(payload);
        self.import_canonical(engine, block, chain).await?;
        Ok(candidate)
    }

    /// Imports and selects the canonical block without starting a build, even after its deadline.
    pub async fn import_canonical(
        &self,
        engine: &(impl BaseEngineApi + Sync),
        block: &Block<BaseTxEnvelope>,
        chain: &impl Upgrades,
    ) -> Result<(), ShadowError> {
        let (canonical, _) = BaseExecutionPayload::from_block_slow(block);
        let beacon_root = block.header.parent_beacon_block_root.unwrap_or_default();
        self.valid(block.header.number, None, || async {
            let status = match canonical.clone() {
                BaseExecutionPayload::V1(payload) => {
                    engine
                        .new_payload_v2(ExecutionPayloadInputV2 {
                            execution_payload: payload,
                            withdrawals: None,
                        })
                        .await?
                }
                BaseExecutionPayload::V2(payload) => {
                    engine.new_payload_v2(payload.into_payload_input_v2(true)).await?
                }
                BaseExecutionPayload::V3(payload) => {
                    engine.new_payload_v3(payload, beacon_root).await?
                }
                BaseExecutionPayload::V4(payload) => {
                    engine.new_payload_v4(payload, beacon_root).await?
                }
            };
            Ok(ForkchoiceUpdated { payload_status: status, payload_id: None })
        })
        .await?;
        self.valid(block.header.number, None, || {
            let state = ForkchoiceState {
                head_block_hash: block.hash_slow(),
                safe_block_hash: block.header.parent_hash,
                finalized_block_hash: block.header.parent_hash,
            };
            async move {
                if chain.is_ecotone_active_at_timestamp(block.header.timestamp) {
                    engine.fork_choice_updated_v3(state, None).await
                } else {
                    engine.fork_choice_updated_v2(state, None).await
                }
            }
        })
        .await?;
        Ok(())
    }

    /// Bounds a complete status-retry phase, including every RPC within it.
    pub async fn valid<F, Fut>(
        &self,
        block_number: u64,
        deadline: Option<SystemTime>,
        call: F,
    ) -> Result<ForkchoiceUpdated, ShadowError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = TransportResult<ForkchoiceUpdated>>,
    {
        tokio::time::timeout(self.timeout, async {
            let mut retry = tokio::time::interval(Duration::from_millis(50));
            loop {
                retry.tick().await;
                if deadline.is_some_and(|deadline| GlobalClock::get().now() >= deadline) {
                    return Err(ShadowError::MissedDeadline(block_number));
                }
                let response = call().await?;
                match response.payload_status.status {
                    PayloadStatusEnum::Valid => return Ok(response),
                    PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted => {}
                    PayloadStatusEnum::Invalid { .. } => {
                        return Err(ShadowError::Invalid {
                            block_number,
                            status: Box::new(response.payload_status),
                        });
                    }
                }
            }
        })
        .await
        .map_err(|_| ShadowError::Timeout("status retry"))?
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        future::{pending, ready},
        time::Duration,
    };

    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};

    use super::{ShadowDriver, ShadowError};

    #[tokio::test]
    async fn retries_when_engine_syncs_or_accepts() {
        let attempt = Cell::new(0);
        let result = ShadowDriver::default()
            .valid(1, None, || {
                let status = [
                    PayloadStatusEnum::Syncing,
                    PayloadStatusEnum::Accepted,
                    PayloadStatusEnum::Valid,
                ][attempt.get()]
                .clone();
                attempt.set(attempt.get() + 1);
                ready(Ok(ForkchoiceUpdated {
                    payload_status: PayloadStatus::from_status(status),
                    payload_id: None,
                }))
            })
            .await
            .unwrap();
        assert!(result.payload_status.is_valid());
        assert_eq!(attempt.get(), 3);
    }

    #[tokio::test]
    async fn times_out_when_engine_never_responds() {
        let driver = ShadowDriver { timeout: Duration::from_millis(1), ..Default::default() };
        let result = driver.valid(1, None, pending).await;
        assert!(matches!(result, Err(ShadowError::Timeout(_))));
    }
}
