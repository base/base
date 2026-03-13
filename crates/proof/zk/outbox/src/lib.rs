#![doc = include_str!("../README.md")]

mod reader;
pub use reader::{OutboxReader, OutboxTask};

mod task_queue;
pub use task_queue::TaskQueue;

mod processor;
pub use processor::OutboxProcessor;

mod database;
pub use database::DatabaseOutboxReader;
