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
    DerivationDelegateConfig, FollowNode, L1Config, L1ConfigBuilder, NodeMode, RollupNode,
    RollupNodeBuilder,
};

mod actors;
pub use actors::{
    AlloyL1BlockFetcher, BlockStream, BootstrapRole, BuildRequest, CancellableContext, Conductor,
    ConductorClient, ConductorError, DelayedL1OriginSelectorProvider, DelegateDerivationActor,
    DelegateL2Client, DelegateL2ClientError, DelegateL2DerivationActor, DerivationActor,
    DerivationActorRequest, DerivationClientError, DerivationClientResult,
    DerivationDelegateClient, DerivationDelegateClientError, DerivationEngineClient,
    DerivationError, DerivationState, DerivationStateMachine, DerivationStateTransitionError,
    DerivationStateUpdate, EngineActor, EngineActorRequest, EngineClientError, EngineClientResult,
    EngineConfig, EngineDerivationClient, EngineError, EngineProcessingRequest, EngineProcessor,
    EngineRequestReceiver, EngineRpcProcessor, EngineRpcRequest, EngineRpcRequestReceiver,
    GetPayloadRequest, GossipTransport, L1BlockFetcher, L1OriginSelector, L1OriginSelectorError,
    L1OriginSelectorProvider, L1WatcherActor, L1WatcherActorError, L1WatcherDerivationClient,
    L1WatcherQueryExecutor, L1WatcherQueryProcessor, L2Finalizer, L2SourceClient, LogRetrier,
    NetworkActor, NetworkActorError, NetworkBuilder, NetworkBuilderError, NetworkConfig,
    NetworkDriver, NetworkDriverError, NetworkEngineClient, NetworkHandler, NetworkInboundData,
    NodeActor, OriginSelector, PayloadBuilder, PayloadSealer, PendingStopSender, PoolActivation,
    QueuedDerivationEngineClient, QueuedEngineDerivationClient, QueuedEngineRpcClient,
    QueuedL1WatcherDerivationClient, QueuedNetworkEngineClient, QueuedSequencerAdminAPIClient,
    QueuedSequencerEngineClient, QueuedUnsafePayloadGossipClient, RecoveryModeGuard, ResetRequest,
    RpcActor, RpcActorError, RpcContext, SealRequest, SealState, SealStepError, SequencerActor,
    SequencerActorError, SequencerAdminQuery, SequencerConfig, SequencerEngineClient,
    UnsafePayloadGossipClient, UnsafePayloadGossipClientError, UnsealedPayloadHandle,
};

mod metrics;
#[cfg(test)]
pub use actors::{
    MockConductor, MockEngineDerivationClient, MockOriginSelector, MockSequencerEngineClient,
    MockUnsafePayloadGossipClient,
};
pub use metrics::Metrics;
