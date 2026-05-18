//! Follow-node delegated forkchoice task and its associated types.

mod error;
pub use error::DelegatedForkchoiceTaskError;

mod task;
pub use task::{DelegatedForkchoiceTask, DelegatedForkchoiceUpdate};

#[cfg(test)]
mod task_test;
