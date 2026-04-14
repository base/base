#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[macro_use]
extern crate tracing;

mod task_queue;
pub use task_queue::{
    BuildTask, BuildTaskError, ConsolidateInput, ConsolidateTask, ConsolidateTaskError,
    DelegatedForkchoiceTask, DelegatedForkchoiceTaskError, DelegatedForkchoiceUpdate, Engine,
    EngineBuildError, EngineResetError, EngineTask, EngineTaskError, EngineTaskErrorSeverity,
    EngineTaskErrors, EngineTaskExt, FinalizeTask, FinalizeTaskError, GetPayloadTask, InsertTask,
    InsertTaskError, SealTask, SealTaskError, SynchronizeTask, SynchronizeTaskError,
};

mod attributes;
pub use attributes::{AttributesMatch, AttributesMismatch};

mod client;
pub use client::{
    BaseEngineClient, EngineClient, EngineClientBuilder, EngineClientError, HyperAuthClient,
};

mod ws_connect;
pub use ws_connect::JwtWsConnect;

mod versions;
pub use versions::{EngineForkchoiceVersion, EngineGetPayloadVersion, EngineNewPayloadVersion};

mod state;
pub use state::{EngineState, EngineSyncState, EngineSyncStateUpdate};

mod kinds;
pub use kinds::EngineKind;

mod query;
pub use query::{EngineQueries, EngineQueriesError, EngineQuerySender};

mod metrics;
pub use metrics::Metrics;

mod sync;
pub use sync::{L2ForkchoiceState, SyncStartError, find_starting_forkchoice};

#[cfg(any(test, feature = "test-utils"))]
/// Utilities that are useful when creating unit tests using structs within this library.
pub mod test_utils;
