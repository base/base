use core::{
    fmt::{self, Display},
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{
    ExecutionData, ForkChoiceUpdateResult, ForkchoiceState, ForkchoiceUpdateError,
    ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum,
};
use base_execution_payload_types::{BasePayloadBuilderAttributes, PayloadBuilderError};
use futures::{FutureExt, TryFutureExt, future::Either};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::{
    BeaconOnNewPayloadError, ExecutionPayload, ForkchoiceStatus, error::BeaconForkChoiceUpdateError,
};

/// Represents the outcome of forkchoice update.
///
/// This is a future that resolves to [`ForkChoiceUpdateResult`]
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Debug)]
pub struct OnForkChoiceUpdated {
    /// Represents the status of the forkchoice update.
    ///
    /// Note: This is separate from the response `fut`, because we still can return an error
    /// depending on the payload attributes, even if the forkchoice update itself is valid.
    forkchoice_status: ForkchoiceStatus,
    /// Returns the result of the forkchoice update.
    fut: Either<futures::future::Ready<ForkChoiceUpdateResult>, PendingPayloadId>,
}

// === impl OnForkChoiceUpdated ===

impl OnForkChoiceUpdated {
    /// Returns the determined status of the received `ForkchoiceState`.
    pub const fn forkchoice_status(&self) -> ForkchoiceStatus {
        self.forkchoice_status
    }

    /// Creates a new instance of `OnForkChoiceUpdated` for the `SYNCING` state
    pub fn syncing() -> Self {
        let status = PayloadStatus::from_status(PayloadStatusEnum::Syncing);
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&status.status),
            fut: Either::Left(futures::future::ready(Ok(ForkchoiceUpdated::new(status)))),
        }
    }

    /// Creates a new instance of `OnForkChoiceUpdated` if the forkchoice update succeeded and no
    /// payload attributes were provided.
    pub fn valid(status: PayloadStatus) -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&status.status),
            fut: Either::Left(futures::future::ready(Ok(ForkchoiceUpdated::new(status)))),
        }
    }

    /// Creates a new instance of `OnForkChoiceUpdated` with the given payload status, if the
    /// forkchoice update failed due to an invalid payload.
    pub fn with_invalid(status: PayloadStatus) -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::from_payload_status(&status.status),
            fut: Either::Left(futures::future::ready(Ok(ForkchoiceUpdated::new(status)))),
        }
    }

    /// Creates a new instance of `OnForkChoiceUpdated` if the forkchoice update failed because the
    /// given state is considered invalid
    pub fn invalid_state() -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::Invalid,
            fut: Either::Left(futures::future::ready(Err(ForkchoiceUpdateError::InvalidState))),
        }
    }

    /// Creates a new instance of `OnForkChoiceUpdated` if the forkchoice update failed because the
    /// requested reorg to the head block exceeds the supported reorg depth.
    pub fn too_deep_reorg() -> Self {
        Self {
            forkchoice_status: ForkchoiceStatus::Invalid,
            fut: Either::Left(futures::future::ready(Err(ForkchoiceUpdateError::TooDeepReorg))),
        }
    }

    /// Creates a new instance of `OnForkChoiceUpdated` if the forkchoice update was successful but
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

impl Future for OnForkChoiceUpdated {
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

/// Additional data for big block payloads that merge multiple real blocks.
///
/// This is used by the `reth_newPayload` endpoint to pass environment switches
/// and prior block hashes needed for correct multi-segment execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BigBlockData<ExecutionData> {
    /// Environment switches at block boundaries.
    /// Each entry is `(cumulative_tx_count, execution_data_of_next_block)`.
    ///
    /// The first entry at index 0 represents the **original unmutated** base block's
    /// `ExecutionData`, which must be used to derive the initial EVM environment.
    pub env_switches: Vec<ExecutionData>,
    /// Block number → real block hash for blocks covered by previous big blocks in a sequence.
    /// When replaying chained big blocks, the BLOCKHASH opcode needs real hashes for blocks
    /// that were merged into earlier big blocks (and thus not individually persisted).
    pub prior_block_hashes: Vec<(u64, alloy_primitives::B256)>,
    /// Block number for this big block.
    pub block_number: u64,
    /// Merged block access list for this big block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_block_access_list: Option<Bytes>,
}

impl ExecutionPayload for BigBlockData<ExecutionData> {
    fn parent_hash(&self) -> B256 {
        self.env_switches[0].parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.env_switches.last().unwrap().block_hash()
    }

    fn block_number(&self) -> u64 {
        self.block_number
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.env_switches[0].withdrawals()
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        self.merged_block_access_list.as_ref()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.env_switches[0].parent_beacon_block_root()
    }

    fn timestamp(&self) -> u64 {
        self.env_switches[0].timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.env_switches.iter().map(|data| data.gas_used()).sum()
    }

    fn gas_limit(&self) -> u64 {
        self.env_switches.iter().map(|data| data.gas_limit()).sum()
    }

    fn transaction_count(&self) -> usize {
        self.env_switches.iter().map(|data| data.transaction_count()).sum()
    }

    fn slot_number(&self) -> Option<u64> {
        self.env_switches[0].payload.slot_number()
    }
}

/// A message for the beacon engine from other components of the node (engine RPC API invoked by the
/// consensus layer).
#[derive(Debug)]
pub enum BeaconEngineMessage {
    /// Message with new payload.
    NewPayload {
        /// The execution payload received by Engine API.
        payload: base_common_rpc_types_engine::ExecutionData,
        /// The sender for returning payload status result.
        tx: oneshot::Sender<Result<PayloadStatus, BeaconOnNewPayloadError>>,
    },
    /// Message with updated forkchoice state.
    ForkchoiceUpdated {
        /// The updated forkchoice state.
        state: ForkchoiceState,
        /// The payload attributes for block building.
        payload_attrs: Option<BasePayloadBuilderAttributes>,
        /// The sender for returning forkchoice updated result.
        tx: oneshot::Sender<Result<OnForkChoiceUpdated, crate::EngineRequestError>>,
    },
}

impl Display for BeaconEngineMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewPayload { payload, .. } => {
                write!(
                    f,
                    "NewPayload(parent: {}, number: {}, hash: {})",
                    payload.parent_hash(),
                    payload.block_number(),
                    payload.block_hash()
                )
            }
            Self::ForkchoiceUpdated { state, payload_attrs, .. } => {
                // we don't want to print the entire payload attributes, because for OP this
                // includes all txs
                write!(
                    f,
                    "ForkchoiceUpdated {{ state: {state:?}, has_payload_attributes: {} }}",
                    payload_attrs.is_some()
                )
            }
        }
    }
}

/// A cloneable sender type that can be used to send engine API messages.
///
/// This type mirrors consensus related functions of the engine API.
#[derive(Debug, Clone)]
pub struct ConsensusEngineHandle {
    to_engine: UnboundedSender<BeaconEngineMessage>,
}

impl ConsensusEngineHandle {
    /// Creates a new beacon consensus engine handle.
    pub const fn new(to_engine: UnboundedSender<BeaconEngineMessage>) -> Self {
        Self { to_engine }
    }

    /// Sends a new payload message to the beacon consensus engine and waits for a response.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/shanghai.md#engine_newpayloadv2>
    pub async fn new_payload(
        &self,
        payload: base_common_rpc_types_engine::ExecutionData,
    ) -> Result<PayloadStatus, BeaconOnNewPayloadError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(BeaconEngineMessage::NewPayload { payload, tx });
        rx.await.map_err(|_| BeaconOnNewPayloadError::EngineUnavailable)?
    }

    /// Sends a forkchoice update message to the beacon consensus engine and waits for a response.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/3d627c95a4d3510a8187dd02e0250ecb4331d27e/src/engine/shanghai.md#engine_forkchoiceupdatedv2>
    pub async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<BasePayloadBuilderAttributes>,
    ) -> Result<ForkchoiceUpdated, BeaconForkChoiceUpdateError> {
        Ok(self
            .send_fork_choice_updated(state, payload_attrs)
            .map_err(|_| BeaconForkChoiceUpdateError::EngineUnavailable)
            .await?
            .map_err(BeaconForkChoiceUpdateError::Internal)?
            .await?)
    }

    /// Sends a forkchoice update message to the beacon consensus engine and returns the receiver to
    /// wait for a response.
    fn send_fork_choice_updated(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<BasePayloadBuilderAttributes>,
    ) -> oneshot::Receiver<Result<OnForkChoiceUpdated, crate::EngineRequestError>> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(BeaconEngineMessage::ForkchoiceUpdated {
            state,
            payload_attrs,
            tx,
        });
        rx
    }
}
