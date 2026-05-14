//! Tasks to update the engine state.

mod task;
pub use task::{EngineTaskError, EngineTaskErrorSeverity, EngineTaskErrors, EngineTaskExt};

mod synchronize;
pub use synchronize::{SynchronizeTask, SynchronizeTaskError};

mod insert;
pub use insert::{InsertPayloadSafety, InsertTaskError, InsertTaskResult};

mod build;
pub use build::{BuildTaskError, EngineBuildError};

mod seal;
pub use seal::{SealTask, SealTaskError};

mod consolidate;
pub use consolidate::{ConsolidateInput, ConsolidateTaskError};

mod delegated_forkchoice;
pub use delegated_forkchoice::{DelegatedForkchoiceTaskError, DelegatedForkchoiceUpdate};

mod finalize;
pub use finalize::FinalizeTaskError;

mod util;
pub(super) use util::{BuildAndSealError, build_and_seal};
