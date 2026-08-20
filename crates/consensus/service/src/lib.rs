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

mod actors;
pub use actors::{
    AfterMailbox, AlloyL1BlockFetcher, AwaitingELSync, AwaitingL1Data, AwaitingSafeHead,
    AwaitingSignal, AwaitingUpdateAfterSignal, BlockStream, BuildOutcome, BuildPipelineState,
    BuildRequest, CancellableContext, CanonicalReconciliationInputs, CanonicalUnsafeCatchup,
    CheckpointActor, CheckpointClient, CheckpointDB, CheckpointError, CheckpointRequest,
    CheckpointWriter, Conductor, ConductorClient, ConductorError, DelayedL1OriginSelectorProvider,
    DelegateDerivationActor, DerivationActor, DerivationActorRequest, DerivationClientError,
    DerivationClientResult, DerivationDelegateClient, DerivationDelegateClientError,
    DerivationEngineClient, DerivationError, DerivationState, Deriving, EngineActor,
    EngineActorRequest, EngineClientError, EngineClientResult, EngineConfig,
    EngineDerivationClient, EngineError, EngineProcessor, EngineRequestReceiver,
    EngineRpcProcessor, EngineRpcRequest, GetPayloadRequest, GossipTransport, Idle,
    InsertUnsafePayloadRequest, L1BlockFetcher, L1OriginSelector, L1OriginSelectorError,
    L1OriginSelectorProvider, L1WatcherActor, L1WatcherActorError, L1WatcherDerivationClient,
    L1WatcherQueryExecutor, L1WatcherQueryProcessor, L2Finalizer, LogRetrier, MailboxIdle,
    NetworkActor, NetworkActorError, NetworkBuilder, NetworkBuilderError, NetworkConfig,
    NetworkDriver, NetworkDriverError, NetworkEngineClient, NetworkHandler, NetworkInboundData,
    NodeActor, NoopCheckpointWriter, OriginSelector, PayloadBuilder, PayloadSealer,
    PendingStopSender, PoolActivation, PrefetchedChainProvider, PrefetchedChainProviderError,
    PreparedL1Origin, ProduceAttributes, QueuedDerivationEngineClient,
    QueuedEngineDerivationClient, QueuedEngineRpcClient, QueuedL1WatcherDerivationClient,
    QueuedNetworkEngineClient, QueuedSequencerAdminAPIClient, QueuedSequencerEngineClient,
    QueuedUnsafePayloadGossipClient, ReconcileShadowRequest, RecoveryModeGuard, ResetOrigin,
    ResetOutcome, ResetReason, ResetRequest, ResetRequestOutcome, RpcActor, RpcActorError,
    RpcContext, SafeHeadCursor, SafeL2SignalRequest, ScheduledTicker, SealState, SealStepError,
    SealStepOutcome, SequencerActor, SequencerActorError, SequencerAdminQuery, SequencerConfig,
    SequencerEngineClient, SequencerEngineRequestCoordinator, SequencerEngineState, ShadowCycle,
    ShadowReconciliationGate, ShadowReconciliationTask, ShadowSequencingState,
    UnsafePayloadGossipClient, UnsafePayloadGossipClientError, UnsealedPayloadHandle,
    UpgradeSignalMetricsActor, UpgradeSignalNodeConfig, ValidatorEngineRequestHandler, WaitOutcome,
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
