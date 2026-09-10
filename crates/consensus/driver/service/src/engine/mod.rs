//! Consensus execution queue, local access, and fork-choice policy.

pub use reth_primitives_traits::SealedBlock;

mod task_queue;
pub use task_queue::{
    BuildTaskError, ConsolidateInput, ConsolidateTask, ConsolidateTaskError, Engine,
    EngineBuildError, EngineResetError, EngineTask, EngineTaskError, EngineTaskErrorSeverity,
    EngineTaskErrors, EngineTaskExt, FinalizeTask, FinalizeTaskError, InsertPayloadPolicy,
    InsertPayloadSafety, InsertTask, InsertTaskError, InsertTaskResult, SealTask, SealTaskError,
    SynchronizeTask, SynchronizeTaskError,
};

mod attributes;
pub use attributes::{AttributesMatch, AttributesMismatch};

mod local_client;
pub use local_client::LocalEngineClient;

mod client;
pub use client::{EngineClient, EngineClientError};

mod state;
pub use state::{EngineState, EngineSyncState, EngineSyncStateUpdate};

mod query;
pub use query::{EngineQueries, EngineQueriesError, EngineQuerySender};

use crate::Metrics;

mod sync;
pub use sync::{
    ForkchoiceCheckpointError, ForkchoiceCheckpointLabel, ForkchoiceCheckpointReader,
    L2ForkchoiceState, NoopForkchoiceCheckpointReader, SyncStartError, find_starting_forkchoice,
    find_starting_forkchoice_with_checkpoint_reader,
};

#[cfg(any(test, feature = "test-utils"))]
/// Utilities that are useful when creating unit tests using structs within this library.
pub mod test_utils;
