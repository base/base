//! Test utilities for the protocol crate.

use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use async_trait::async_trait;
use op_alloy_consensus::OpBlock;
use spin::Mutex;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

use crate::{BatchValidationProvider, L2BlockInfo};

/// An error for implementations of the [BatchValidationProvider] trait.
#[derive(Debug, thiserror::Error)]
pub enum TestBatchValidatorError {
    /// The block was not found.
    #[error("Block not found")]
    BlockNotFound,
    /// The L2 block was not found.
    #[error("L2 Block not found")]
    L2BlockNotFound,
}

/// An [TestBatchValidator] implementation for testing.
#[derive(Debug, Default, Clone)]
pub struct TestBatchValidator {
    /// Blocks
    pub blocks: Vec<L2BlockInfo>,
    /// Short circuit the block return to be the first block.
    pub short_circuit: bool,
    /// Blocks
    pub op_blocks: Vec<OpBlock>,
}

impl TestBatchValidator {
    /// Creates a new []TestBatchValidator with the given origin and batches.
    pub const fn new(blocks: Vec<L2BlockInfo>, op_blocks: Vec<OpBlock>) -> Self {
        Self { blocks, short_circuit: false, op_blocks }
    }
}

#[async_trait]
impl BatchValidationProvider for TestBatchValidator {
    type Error = TestBatchValidatorError;

    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        if self.short_circuit {
            return self
                .blocks
                .first()
                .copied()
                .ok_or_else(|| TestBatchValidatorError::BlockNotFound);
        }
        self.blocks
            .iter()
            .find(|b| b.block_info.number == number)
            .cloned()
            .ok_or_else(|| TestBatchValidatorError::BlockNotFound)
    }

    async fn block_by_number(&mut self, number: u64) -> Result<OpBlock, Self::Error> {
        self.op_blocks
            .iter()
            .find(|p| p.header.number == number)
            .cloned()
            .ok_or_else(|| TestBatchValidatorError::L2BlockNotFound)
    }
}

/// The storage for the collected traces.
#[derive(Debug, Default, Clone)]
pub struct TraceStorage(pub Arc<Mutex<Vec<(Level, String)>>>);

impl TraceStorage {
    /// Returns the items in the storage that match the specified level.
    pub fn get_by_level(&self, level: Level) -> Vec<String> {
        self.0
            .lock()
            .iter()
            .filter_map(|(l, message)| if *l == level { Some(message.clone()) } else { None })
            .collect()
    }

    /// Returns if the storage is empty.
    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }
}

/// A subscriber layer that collects traces and their log levels.
#[derive(Debug, Default)]
pub struct CollectingLayer {
    /// The storage for the collected traces.
    pub storage: TraceStorage,
}

impl CollectingLayer {
    /// Creates a new collecting layer with the specified storage.
    pub const fn new(storage: TraceStorage) -> Self {
        Self { storage }
    }
}

impl<S: Subscriber> Layer<S> for CollectingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = *metadata.level();
        let message = format!("{:?}", event);

        let mut storage = self.storage.0.lock();
        storage.push((level, message));
    }
}
