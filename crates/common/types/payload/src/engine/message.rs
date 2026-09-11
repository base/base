use core::{
    fmt::{self, Display},
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::{FutureExt, future::Either};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::{
    BasePayloadBuilderAttributes, ForkChoiceUpdateResult, ForkchoiceState, ForkchoiceUpdateError,
    ForkchoiceUpdated, PayloadBuilderError, PayloadId, PayloadStatus, PayloadStatusEnum,
    engine::{BeaconOnNewPayloadError, ForkchoiceStatus, error::BeaconForkChoiceUpdateError},
};

/// Represents the outcome of forkchoice update.
///
/// This is a future that resolves to [`ForkChoiceUpdateResult`]
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Debug)]
pub struct PendingHeadUpdate {
    /// Represents the status of the forkchoice update.
    ///
    /// Note: This is separate from the response `fut`, because we still can return an error
    /// depending on the payload attributes, even if the forkchoice update itself is valid.
    forkchoice_status: ForkchoiceStatus,
    /// Returns the result of the forkchoice update.
    fut: Either<futures::future::Ready<ForkChoiceUpdateResult>, PendingPayloadId>,
}

// === impl PendingHeadUpdate ===

impl PendingHeadUpdate {
    /// Returns the determined status of the received `ForkchoiceState`.
    pub const fn forkchoice_status(&self) -> ForkchoiceStatus {
        self.forkchoice_status
    }

    /// Creates a new instance of `PendingHeadUpdate` for the `SYNCING` state
    pub fn syncing() -> Self {
        let status = PayloadStatus::from_status(PayloadStatusEnum::Syncing);
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&status.status),
            fut: Either::Left(futures::future::ready(Ok(ForkchoiceUpdated::new(status)))),
        }
    }

    /// Creates a new instance of `PendingHeadUpdate` if the forkchoice update succeeded and no
    /// payload attributes were provided.
    pub fn valid(status: PayloadStatus) -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&status.status),
            fut: Either::Left(futures::future::ready(Ok(ForkchoiceUpdated::new(status)))),
        }
    }

    /// Creates a new instance of `PendingHeadUpdate` with the given payload status, if the
    /// forkchoice update failed due to an invalid payload.
    pub fn with_invalid(status: PayloadStatus) -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&status.status),
            fut: Either::Left(futures::future::ready(Ok(ForkchoiceUpdated::new(status)))),
        }
    }

    /// Creates a new instance of `PendingHeadUpdate` if the forkchoice update failed because the
    /// given state is considered invalid
    pub fn invalid_state() -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::Invalid,
            fut: Either::Left(futures::future::ready(Err(ForkchoiceUpdateError::InvalidState))),
        }
    }

    /// Creates a new instance of `PendingHeadUpdate` if the forkchoice update failed because the
    /// requested reorg to the head block exceeds the supported reorg depth.
    pub fn too_deep_reorg() -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::Invalid,
            fut: Either::Left(futures::future::ready(Err(ForkchoiceUpdateError::TooDeepReorg))),
        }
    }

    /// Creates a new instance of `PendingHeadUpdate` if the forkchoice update was successful but
    /// payload attributes were invalid.
    pub fn invalid_payload_attributes() -> Self {
        Self {
            // This is valid because this is only reachable if the state and payload is valid
            forkchoice_status: ForkchoiceStatus::Valid,
            fut: Either::Left(futures::future::ready(Err(
                ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes,
            ))),
        }
    }

    /// If the forkchoice update was successful and no payload attributes were provided, this method
    pub const fn updated_with_pending_payload_id(
        payload_status: PayloadStatus,
        pending_payload_id: oneshot::Receiver<Result<PayloadId, PayloadBuilderError>>,
    ) -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&payload_status.status),
            fut: Either::Right(PendingPayloadId {
                payload_status: Some(payload_status),
                pending_payload_id,
            }),
        }
    }
}

impl Future for PendingHeadUpdate {
    type Output = ForkChoiceUpdateResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().fut.poll_unpin(cx)
    }
}

/// A future that returns the payload id of a yet to be initiated payload job after a successful
/// forkchoice update
#[derive(Debug)]
struct PendingPayloadId {
    payload_status: Option<PayloadStatus>,
    pending_payload_id: oneshot::Receiver<Result<PayloadId, PayloadBuilderError>>,
}

impl Future for PendingPayloadId {
    type Output = ForkChoiceUpdateResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let res = ready!(this.pending_payload_id.poll_unpin(cx));
        match res {
            Ok(Ok(payload_id)) => Poll::Ready(Ok(ForkchoiceUpdated {
                payload_status: this.payload_status.take().expect("Polled after completion"),
                payload_id: Some(payload_id),
            })),
            Err(_) | Ok(Err(_)) => {
                // failed to initiate a payload build job
                Poll::Ready(Err(ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes))
            }
        }
    }
}

/// Acknowledgment of a native head-changing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadUpdateOutcome {
    /// Execution can serve the requested canonical head.
    Applied {
        /// Acknowledged canonical head.
        head: alloy_primitives::B256,
    },
    /// The requested head is not yet available; callers must not acknowledge insertion.
    Syncing,
}

impl HeadUpdateOutcome {
    /// Converts the tree's validation result into a native acknowledgment.
    pub fn from_status(
        status: PayloadStatus,
        head: alloy_primitives::B256,
    ) -> Result<Self, BeaconForkChoiceUpdateError> {
        match status.status {
            PayloadStatusEnum::Valid => Ok(Self::Applied { head }),
            PayloadStatusEnum::Syncing => Ok(Self::Syncing),
            _ => Err(BeaconForkChoiceUpdateError::InvalidHeads(status)),
        }
    }

    /// Whether the execution engine acknowledged canonicalization.
    pub const fn is_applied(self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// Converts an acknowledgment for reference-client comparison fixtures.
    pub const fn into_payload_status(self) -> PayloadStatus {
        match self {
            Self::Applied { head } => {
                PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(head) }
            }
            Self::Syncing => PayloadStatus::from_status(PayloadStatusEnum::Syncing),
        }
    }
}

/// A serialized request to the execution tree.
#[derive(Debug)]
pub enum ExecutionCommand {
    /// Import a payload and select its canonical heads.
    AppendPayload {
        /// Payload to execute.
        payload: Box<crate::ExecutionData>,
        /// Heads to apply after importing the payload.
        heads: ForkchoiceState,
        /// Debug injection: skip importing the payload.
        skip_import: bool,
        /// Debug injection: skip applying the heads.
        skip_heads: bool,
        /// Completion after both stages have run.
        tx: oneshot::Sender<Result<HeadUpdateOutcome, AppendPayloadError>>,
    },
    /// Select a parent and begin building on it.
    StartBuilding {
        /// Heads containing the build parent.
        heads: ForkchoiceState,
        /// Attributes for the new build.
        attributes: Box<BasePayloadBuilderAttributes>,
        /// Build completion receiver, resolved outside the tree thread.
        tx: oneshot::Sender<Result<PendingHeadUpdate, crate::EngineRequestError>>,
    },
    /// Select heads without starting a build.
    UpdateHeads {
        /// Complete head selection.
        heads: ForkchoiceState,
        /// Head application response.
        tx: oneshot::Sender<Result<PendingHeadUpdate, crate::EngineRequestError>>,
    },
}

/// Failures importing and canonicalizing a payload, distinguished by stage.
#[derive(Debug, thiserror::Error)]
pub enum AppendPayloadError {
    /// Payload execution failed.
    #[error(transparent)]
    Import(#[from] BeaconOnNewPayloadError),
    /// Payload validation rejected the block.
    #[error("invalid payload: {0:?}")]
    InvalidPayload(PayloadStatus),
    /// Applying the requested heads failed.
    #[error(transparent)]
    Heads(#[from] BeaconForkChoiceUpdateError),
}

impl Display for ExecutionCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppendPayload { payload, heads, .. } => {
                write!(f, "AppendPayload({}, {heads:?})", payload.block_hash())
            }
            Self::StartBuilding { heads, .. } => write!(f, "StartBuilding({heads:?})"),
            Self::UpdateHeads { heads, .. } => write!(f, "UpdateHeads({heads:?})"),
        }
    }
}

/// A cloneable sender for serialized execution operations.
#[derive(Debug, Clone)]
pub struct ConsensusEngineHandle {
    /// Sender to the execution tree.
    pub to_engine: UnboundedSender<ExecutionCommand>,
}

impl ConsensusEngineHandle {
    /// Connects to an execution command stream.
    pub const fn new(to_engine: UnboundedSender<ExecutionCommand>) -> Self {
        Self { to_engine }
    }

    /// Imports a payload and applies its heads before acknowledging it.
    pub async fn append_payload(
        &self,
        payload: crate::ExecutionData,
        heads: ForkchoiceState,
    ) -> Result<HeadUpdateOutcome, AppendPayloadError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(ExecutionCommand::AppendPayload {
            payload: Box::new(payload),
            heads,
            skip_import: false,
            skip_heads: false,
            tx,
        });
        rx.await.map_err(|_| BeaconOnNewPayloadError::EngineUnavailable)?
    }

    /// Applies a complete head selection without building.
    pub async fn update_heads(
        &self,
        heads: ForkchoiceState,
    ) -> Result<HeadUpdateOutcome, BeaconForkChoiceUpdateError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(ExecutionCommand::UpdateHeads { heads, tx });
        let response = rx
            .await
            .map_err(|_| BeaconForkChoiceUpdateError::EngineUnavailable)?
            .map_err(BeaconForkChoiceUpdateError::Internal)?
            .await?;
        HeadUpdateOutcome::from_status(response.payload_status, heads.head_block_hash)
    }

    /// Applies the build parent and returns the required build identifier.
    pub async fn start_building(
        &self,
        heads: ForkchoiceState,
        attributes: BasePayloadBuilderAttributes,
    ) -> Result<PayloadId, BeaconForkChoiceUpdateError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(ExecutionCommand::StartBuilding {
            heads,
            attributes: Box::new(attributes),
            tx,
        });
        let response = rx
            .await
            .map_err(|_| BeaconForkChoiceUpdateError::EngineUnavailable)?
            .map_err(BeaconForkChoiceUpdateError::Internal)?
            .await?;
        if response.payload_status.is_syncing() {
            return Err(BeaconForkChoiceUpdateError::Syncing);
        }
        if !response.is_valid() {
            return Err(BeaconForkChoiceUpdateError::InvalidHeads(response.payload_status));
        }
        response.payload_id.ok_or(BeaconForkChoiceUpdateError::MissingBuild)
    }
}
