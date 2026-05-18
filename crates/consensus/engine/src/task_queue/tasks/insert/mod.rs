//! Task to insert a payload into the execution engine.

mod task;
pub use task::{InsertPayloadSafety, InsertTask, InsertTaskResult};

mod error;
pub use error::InsertTaskError;
