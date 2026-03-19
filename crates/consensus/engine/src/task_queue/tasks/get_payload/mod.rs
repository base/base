//! A task for fetching a sealed payload from the engine without inserting it.

mod task;
pub use task::GetPayloadTask;

#[cfg(test)]
mod task_test;
