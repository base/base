//! Tasks to update the engine state.

mod task;
pub use task::{
    EngineTask, EngineTaskError, EngineTaskErrorSeverity, EngineTaskErrors, EngineTaskExt,
};

mod synchronize;
pub use synchronize::{SynchronizeTask, SynchronizeTaskError};

mod insert;
pub use insert::{
    InsertPayloadPolicy, InsertPayloadSafety, InsertTask, InsertTaskError, InsertTaskResult,
};

mod build;
pub use build::{BuildTaskError, EngineBuildError};

mod seal;
pub use seal::SealTaskError;

mod consolidate;
pub use consolidate::{ConsolidateInput, ConsolidateTask, ConsolidateTaskError};

mod finalize;
pub use finalize::{FinalizeTask, FinalizeTaskError};

mod util;
pub use util::{BuildAndAppend, BuildAndAppendError};
