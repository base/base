#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[macro_use]
extern crate tracing;

mod service;
pub use service::{
    DerivationDelegateConfig, FollowNode, FollowNodeConfig, HEAD_STREAM_POLL_INTERVAL, L1Config,
    L1ConfigBuilder, NodeMode, RollupNode, RollupNodeBuilder, ShutdownSignal,
    UpgradeSignalBuilderConfig,
};

mod follow;
pub use follow::{FollowError, RemoteClient, RemoteL2Client, RemoteL2ClientError};

mod standalone;
pub use standalone::{
    StandaloneAttributesBuilder, StandaloneDerivationClient, StandaloneOriginSelector,
    StandalonePrefund, StandaloneSequencerNode, StandaloneUnsafePayloadGossipClient,
};

mod actors;
pub use actors::{
    AlloyL1BlockFetcher, BlockStream, BuildOutcome, BuildPipelineState, BuildRequest,
    CancellableContext, CanonicalReconciliationInputs, CanonicalUnsafeCatchup, CheckpointActor,
    CheckpointClient, CheckpointDB, CheckpointError, CheckpointRequest, CheckpointWriter,
    Conductor, ConductorClient, ConductorError, DelayedL1OriginSelectorProvider,
    DelegateDerivationActor, DerivationActor, DerivationActorRequest, DerivationClientError,
    DerivationClientResult, DerivationDelegateClient, DerivationDelegateClientError,
    DerivationEngineClient, DerivationError, DerivationState, DerivationStateMachine,
    DerivationStateTransitionError, DerivationStateUpdate, EngineActor, EngineActorRequest,
    EngineClientError, EngineClientResult, EngineConfig, EngineDerivationClient, EngineError,
    EngineProcessor, EngineRequestReceiver, EngineRpcClient, EngineRpcProcessor, EngineRpcRequest,
    GetPayloadRequest, GossipTransport, InsertUnsafePayloadRequest, L1BlockFetcher,
    L1OriginSelector, L1OriginSelectorError, L1OriginSelectorProvider, L1WatcherActor,
    L1WatcherActorError, L1WatcherDerivationClient, L1WatcherQueryExecutor,
    L1WatcherQueryProcessor, L2Finalizer, LogRetrier, NetworkActor, NetworkActorError,
    NetworkBuilder, NetworkBuilderError, NetworkConfig, NetworkDriver, NetworkDriverError,
    NetworkEngineClient, NetworkHandler, NetworkInboundData, NodeActor, NoopCheckpointWriter,
    OriginSelector, PayloadBuilder, PayloadSealer, PendingStopSender, PoolActivation,
    PrefetchedChainProvider, PrefetchedChainProviderError, PreparedL1Origin,
    QueuedDerivationEngineClient, QueuedEngineDerivationClient, QueuedL1WatcherDerivationClient,
    QueuedNetworkEngineClient, QueuedSequencerEngineClient, QueuedUnsafePayloadGossipClient,
    ReconcileShadowRequest, RecoveryModeGuard, ResetOrigin, ResetOutcome, ResetReason,
    ResetRequest, ResetRequestOutcome, RpcActor, RpcActorError, RpcContext, ScheduledTicker,
    SealState, SealStepError, SealStepOutcome, SequencerActor, SequencerActorError,
    SequencerAdminClient, SequencerAdminQuery, SequencerConfig, SequencerEngineClient,
    SequencerEngineRequestCoordinator, SequencerEngineState, ShadowCycle, ShadowFunding,
    ShadowReconciliationGate, ShadowReconciliationTask, ShadowSequencingState,
    UnsafePayloadGossipClient, UnsafePayloadGossipClientError, UnsealedPayloadHandle,
    UpgradeSignalMetricsActor, UpgradeSignalNodeConfig, ValidatorEngineRequestHandler,
};

mod metrics;
#[cfg(test)]
pub mod test_utils;
#[cfg(test)]
pub use actors::{
    MockConductor, MockEngineDerivationClient, MockOriginSelector, MockSequencerEngineClient,
    MockUnsafePayloadGossipClient,
};
#[cfg(test)]
pub use follow::MockRemoteClient;
pub use metrics::Metrics;

mod rpc;
pub use rpc::{
    AdminRpc, BaseRpc, DevEngineRpc, HealthzRpc, L1State, L1WatcherQueries, L1WatcherQuerySender,
    NetworkAdminQuery, P2pRpc, RollupRpc, RpcBuilder, SequencerAdminAPIError, WsRPC,
};

mod safedb;
pub use safedb::{
    DisabledSafeDB, SafeDB, SafeDBError, SafeDBReader, SafeHeadListener, SafeHeadResponse,
};

mod engine;
#[cfg(any(test, feature = "test-utils"))]
pub use engine::test_utils as engine_test_utils;
pub use engine::{
    AttributesMatch, AttributesMismatch, BuildTaskError, ConsolidateInput, ConsolidateTask,
    ConsolidateTaskError, Engine, EngineBuildError, EngineClient,
    EngineClientError as ExecutionClientError, EngineQueries, EngineQueriesError,
    EngineQuerySender, EngineResetError, EngineState, EngineSyncState, EngineSyncStateUpdate,
    EngineTask, EngineTaskError, EngineTaskErrorSeverity, EngineTaskErrors, EngineTaskExt,
    FinalizeTask, FinalizeTaskError, ForkchoiceCheckpointError, ForkchoiceCheckpointLabel,
    ForkchoiceCheckpointReader, InsertPayloadPolicy, InsertPayloadSafety, InsertTask,
    InsertTaskError, InsertTaskResult, L2ForkchoiceState, LocalEngineClient,
    NoopForkchoiceCheckpointReader, SealTask, SealTaskError, SealedBlock, SyncStartError,
    SynchronizeTask, SynchronizeTaskError, find_starting_forkchoice,
    find_starting_forkchoice_with_checkpoint_reader,
};
