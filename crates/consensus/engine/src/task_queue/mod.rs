//! The [`Engine`] task queue and the [`EngineTask`]s it can execute.

mod core;
pub use core::{Engine, EngineResetError};

mod tasks;
pub(crate) use tasks::build_and_seal;
pub use tasks::{
    BuildTask, BuildTaskError, ConsolidateInput, ConsolidateTask, ConsolidateTaskError,
    EngineBuildError, EngineTask, EngineTaskError, EngineTaskErrorSeverity, EngineTaskErrors,
    EngineTaskExt, FinalizeTask, FinalizeTaskError, InsertTask, InsertTaskError, SealTask,
    SealTaskError, SynchronizeTask, SynchronizeTaskError,
};
