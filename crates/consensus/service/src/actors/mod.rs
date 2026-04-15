//! [`NodeActor`] services for the node.
//!
//! [NodeActor]: super::NodeActor

mod traits;
pub use traits::{CancellableContext, NodeActor};

mod engine;
#[cfg(test)]
pub use engine::MockEngineDerivationClient;
pub use engine::{
    BootstrapRole, BuildRequest, EngineActor, EngineActorRequest, EngineClientError,
    EngineClientResult, EngineConfig, EngineDerivationClient, EngineError, EngineProcessingRequest,
    EngineProcessor, EngineRequestReceiver, EngineRpcProcessor, EngineRpcRequest,
    EngineRpcRequestReceiver, GetPayloadRequest, QueuedEngineDerivationClient, ResetRequest,
    SealRequest,
};

mod rpc;
pub use rpc::{
    QueuedEngineRpcClient, QueuedSequencerAdminAPIClient, RpcActor, RpcActorError, RpcContext,
};

mod derivation;
pub use derivation::{
    DelegateDerivationActor, DelegateL2Client, DelegateL2ClientError, DelegateL2DerivationActor,
    DerivationActor, DerivationActorRequest, DerivationClientError, DerivationClientResult,
    DerivationDelegateClient, DerivationDelegateClientError, DerivationEngineClient,
    DerivationError, DerivationState, DerivationStateMachine, DerivationStateTransitionError,
    DerivationStateUpdate, L2Finalizer, L2SourceClient, QueuedDerivationEngineClient,
};

mod l1_watcher;
pub use l1_watcher::{
    AlloyL1BlockFetcher, BlockStream, L1BlockFetcher, L1WatcherActor, L1WatcherActorError,
    L1WatcherDerivationClient, L1WatcherQueryExecutor, L1WatcherQueryProcessor, LogRetrier,
    QueuedL1WatcherDerivationClient,
};

mod network;
pub use network::{
    GossipTransport, NetworkActor, NetworkActorError, NetworkBuilder, NetworkBuilderError,
    NetworkConfig, NetworkDriver, NetworkDriverError, NetworkEngineClient, NetworkHandler,
    NetworkInboundData, QueuedNetworkEngineClient, QueuedUnsafePayloadGossipClient,
    UnsafePayloadGossipClient, UnsafePayloadGossipClientError,
};

mod sequencer;
#[cfg(test)]
pub use network::MockUnsafePayloadGossipClient;
pub use sequencer::{
    Conductor, ConductorClient, ConductorError, DelayedL1OriginSelectorProvider, L1OriginSelector,
    L1OriginSelectorError, L1OriginSelectorProvider, OriginSelector, PayloadBuilder, PayloadSealer,
    PendingStopSender, PoolActivation, QueuedSequencerEngineClient, RecoveryModeGuard, SealState,
    SealStepError, SequencerActor, SequencerActorError, SequencerAdminQuery, SequencerConfig,
    SequencerEngineClient, UnsealedPayloadHandle,
};
#[cfg(test)]
pub use sequencer::{MockConductor, MockOriginSelector, MockSequencerEngineClient};
