//! Follow-node delegated forkchoice update and error types.

mod error;
pub use error::DelegatedForkchoiceTaskError;

mod update;
pub use update::DelegatedForkchoiceUpdate;

#[cfg(test)]
mod direct_test;
