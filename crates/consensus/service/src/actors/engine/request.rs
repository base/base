use alloy_rpc_types_engine::PayloadId;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::{BuildTaskError, EngineQueries, InsertTaskError, SealTaskError};
use base_protocol::{AttributesWithParent, L2BlockInfo};
use opentelemetry::Context;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

/// The result of an Engine client call.
pub type EngineClientResult<T> = Result<T, EngineClientError>;

/// Error making requests to the `BlockEngine`.
#[derive(Debug, Error)]
pub enum EngineClientError {
    /// Error making a request to the engine. The request never made it there.
    #[error("Error making a request to the engine: {0}.")]
    RequestError(String),

    /// Error receiving response from the engine.
    /// This means the request may or may not have succeeded.
    #[error("Error receiving response from the engine: {0}.")]
    ResponseError(String),

    /// An error occurred starting to build a block.
    #[error(transparent)]
    StartBuildError(#[from] BuildTaskError),

    /// An error occurred sealing a block.
    #[error(transparent)]
    SealError(#[from] SealTaskError),

    /// An error occurred inserting an unsafe block.
    #[error(transparent)]
    InsertError(#[from] InsertTaskError),

    /// An error occurred performing the reset.
    #[error("An error occurred performing the reset: {0}.")]
    ResetForkchoiceError(String),

    /// EL sync or canonical catch-up is incomplete; the reset cannot proceed yet.
    #[error("EL sync or canonical catch-up in progress; reset deferred")]
    ELSyncing,

    /// Shadow reconciliation is unavailable in this mode.
    #[error("shadow reconciliation is disabled")]
    ShadowReconciliationDisabled,

    /// The canonical reconciliation payload buffer can no longer be reconciled safely.
    #[error("canonical reconciliation payload buffer is faulted")]
    ShadowBufferFaulted,

    /// The requested shadow reconciliation range or payload chain is invalid.
    #[error("invalid shadow reconciliation range: {0}")]
    InvalidShadowReconciliation(String),

    /// A deferred safe or finalized forkchoice update failed during reconciliation.
    #[error("shadow reconciliation forkchoice update failed: {0}")]
    ShadowForkchoiceUpdate(String),
}

/// Inbound requests that the [`crate::EngineActor`] can process.
#[derive(Debug)]
pub enum EngineActorRequest {
    /// Request to build.
    BuildRequest(Box<BuildRequest>),
    /// Request to get the sealed payload without inserting it.
    GetPayloadRequest(Box<GetPayloadRequest>),
    /// Request to consolidate using a safe L2 signal from derived attributes or delegated
    /// safe-block derivation.
    ProcessSafeL2SignalRequest(Box<SafeL2SignalRequest>),
    /// Request to finalize the L2 block at the provided block number.
    ProcessFinalizedL2BlockNumberRequest(Box<u64>),
    /// Request to process an unsafe block authenticated by the P2P gossip layer.
    ProcessUnsafeL2BlockRequest(Box<BaseExecutionPayloadEnvelope>),
    /// Request to insert an unsafe block supplied through the admin API.
    ProcessAdminUnsafeL2BlockRequest(Box<BaseExecutionPayloadEnvelope>),
    /// Request to insert a locally produced sequencer unsafe block.
    ProcessLocalUnsafeL2BlockRequest(Box<InsertUnsafePayloadRequest>),
    /// Reconcile private blocks to an authenticated P2P payload range.
    ReconcileShadowRequest(Box<ReconcileShadowRequest>),
    /// Request to reset engine forkchoice.
    ResetRequest(Box<ResetRequest>),
}

/// Consolidation request from derivation (or tests) to the engine.
///
/// Direct derivation always pairs attributes with a confirmation oneshot. Delegated
/// derivation and tests promote a safe L2 head without a waiter.
pub enum SafeL2SignalRequest {
    /// Derived attributes whose confirmation is owned by [`crate::AwaitingSafeHead`].
    Derived {
        /// Attributes to consolidate.
        attributes: Box<AttributesWithParent>,
        /// Completes after the drain that runs this consolidate task.
        confirmed: oneshot::Sender<L2BlockInfo>,
    },
    /// Delegated or test-driven safe-head promotion. Confirmation is mailbox-only.
    Delegated {
        /// Safe L2 head to consolidate.
        safe_l2: L2BlockInfo,
    },
}

impl std::fmt::Debug for SafeL2SignalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Derived { attributes, .. } => f
                .debug_struct("Derived")
                .field("attributes", attributes)
                .field("confirmed", &"oneshot::Sender")
                .finish(),
            Self::Delegated { safe_l2 } => {
                f.debug_struct("Delegated").field("safe_l2", safe_l2).finish()
            }
        }
    }
}

impl SafeL2SignalRequest {
    /// Derived-attribute consolidation with a lock-step confirmation oneshot.
    pub fn derived(
        attributes: AttributesWithParent,
        confirmed: oneshot::Sender<L2BlockInfo>,
    ) -> Self {
        Self::Derived { attributes: Box::new(attributes), confirmed }
    }

    /// Delegated or test consolidation of an already-known safe L2 head.
    pub const fn delegated(safe_l2: L2BlockInfo) -> Self {
        Self::Delegated { safe_l2 }
    }
}

/// Request to replace a private shadow range with the active sequencer's P2P branch.
#[derive(Debug)]
pub struct ReconcileShadowRequest {
    /// Last head built by the shadow sequencer before reconciliation.
    pub shadow_head: L2BlockInfo,
    /// Channel on which readiness, success, or failure is returned.
    pub result_tx: mpsc::Sender<EngineClientResult<Option<L2BlockInfo>>>,
}

/// RPC Request for the engine to handle.
#[derive(Debug)]
pub enum EngineRpcRequest {
    /// Engine RPC query.
    EngineQuery(Box<EngineQueries>),
}

/// A request to build a payload.
/// Contains the attributes to build and a channel to send back the resulting `PayloadId`.
#[derive(Debug)]
pub struct BuildRequest {
    /// The [`AttributesWithParent`] from which the block build should be started.
    pub attributes: AttributesWithParent,
    /// The channel on which the result, successful or not, will be sent.
    pub result_tx: mpsc::Sender<Result<PayloadId, BuildTaskError>>,
    /// [`opentelemetry::Context`] from the requester, for trace propagation.
    pub otel_cx: Context,
}

/// A request to reset engine forkchoice or complete coordinated shadow activation.
/// Optionally contains a channel to send back the response if the caller would like to know that
/// the request was successfully processed.
#[derive(Debug)]
pub struct ResetRequest {
    /// response will be sent to this channel, if `Some`.
    pub result_tx: mpsc::Sender<EngineClientResult<()>>,
    /// The subsystem and coordination path that requested the reset.
    pub origin: ResetOrigin,
    /// The condition that caused the reset request.
    pub reason: ResetReason,
}

/// Identifies the state owner coordinating an engine reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOrigin {
    /// Derivation requested pipeline recovery.
    Derivation,
    /// The sequencer requested its ordinary startup or recovery reset.
    Sequencer,
    /// The shadow sequencer coordinated initial activation or its private-cycle reset.
    ShadowCycleCoordinated,
}

impl ResetOrigin {
    /// Returns the bounded metrics label for this reset origin.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Derivation => "derivation",
            Self::Sequencer => "sequencer",
            Self::ShadowCycleCoordinated => "shadow_cycle_coordinated",
        }
    }
}

/// Identifies the condition that caused an engine reset request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// The derivation pipeline encountered a generic reset condition.
    DerivationPipeline,
    /// Derivation detected an L1 reorganization.
    DerivationL1Reorg,
    /// The accepted L1 origin could not be fetched.
    L1OriginUnavailable,
    /// The canonical L1 successor does not extend the accepted origin.
    L1OriginOrphaned,
    /// The selected L1 origin is inconsistent with the unsafe L2 head.
    L1OriginInconsistent,
    /// The sequencer is performing its initial engine reset.
    SequencerStartup,
    /// An administrator explicitly requested a reset.
    Admin,
    /// A shadow sequencer is beginning a new private production cycle.
    ShadowCycle,
}

impl ResetReason {
    /// Returns the bounded metrics label for this reset reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DerivationPipeline => "derivation_pipeline",
            Self::DerivationL1Reorg => "derivation_l1_reorg",
            Self::L1OriginUnavailable => "l1_origin_unavailable",
            Self::L1OriginOrphaned => "l1_origin_orphaned",
            Self::L1OriginInconsistent => "l1_origin_inconsistent",
            Self::SequencerStartup => "sequencer_startup",
            Self::Admin => "admin",
            Self::ShadowCycle => "shadow_cycle",
        }
    }
}

/// Outcome of one engine reset request handling attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetRequestOutcome {
    /// The reset completed without changing the unsafe L2 head.
    Unchanged,
    /// The reset changed the unsafe L2 head.
    Rewound,
    /// The reset was deferred until engine synchronization completes.
    Deferred,
    /// The engine reset completed, but derivation could not be notified.
    DerivationNotificationFailed,
    /// The engine reset did not complete.
    Failed,
}

impl ResetRequestOutcome {
    /// Classifies a successful reset from its unsafe heads before and after processing.
    pub fn from_unsafe_heads(before: L2BlockInfo, after: L2BlockInfo) -> Self {
        if before.block_info.number == after.block_info.number
            && before.block_info.hash == after.block_info.hash
        {
            Self::Unchanged
        } else {
            Self::Rewound
        }
    }

    /// Returns the bounded metrics label for this reset outcome.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Rewound => "rewound",
            Self::Deferred => "deferred",
            Self::DerivationNotificationFailed => "derivation_notification_failed",
            Self::Failed => "failed",
        }
    }
}

/// A request to insert a local unsafe payload.
#[derive(Debug)]
pub struct InsertUnsafePayloadRequest {
    /// The payload envelope to insert.
    pub envelope: BaseExecutionPayloadEnvelope,
    /// Optional response channel used by the sequencer to wait for actual insertion.
    pub result_tx: Option<mpsc::Sender<Result<L2BlockInfo, InsertTaskError>>>,
    /// [`opentelemetry::Context`] from the requester, for trace propagation.
    pub otel_cx: Context,
}

/// A request to get the sealed payload without inserting it into the engine.
/// Contains the `PayloadId`, attributes, and a channel to send back the result.
#[derive(Debug)]
pub struct GetPayloadRequest {
    /// The `PayloadId` to fetch.
    pub payload_id: PayloadId,
    /// The attributes associated with the payload.
    pub attributes: AttributesWithParent,
    /// The channel on which the result, successful or not, will be sent.
    pub result_tx: mpsc::Sender<Result<BaseExecutionPayloadEnvelope, SealTaskError>>,
    /// [`opentelemetry::Context`] from the requester, for trace propagation.
    pub otel_cx: Context,
}
