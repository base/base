//! Derivation actors including direct, delegated, and L2-delegate variants.

mod actor;
pub use actor::{DerivationActor, DerivationError, ProduceAttributes, WaitOutcome};

mod delegated;
pub use delegated::{
    DelegateDerivationActor, DerivationDelegateClient, DerivationDelegateClientError,
};

mod engine_client;
pub use engine_client::{DerivationEngineClient, QueuedDerivationEngineClient};

mod finalizer;
pub use finalizer::L2Finalizer;

mod request;
pub use request::{DerivationActorRequest, DerivationClientError, DerivationClientResult};

mod state_machine;
pub use state_machine::{
    AfterMailbox, AwaitingELSync, AwaitingL1Data, AwaitingSafeHead, AwaitingSignal,
    AwaitingUpdateAfterSignal, DerivationState, Deriving, Idle, MailboxIdle, SafeHeadCursor,
};
