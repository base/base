//! This module contains each stage of the derivation pipeline.
//!
//! It offers a high-level API to functionally apply each stage's output as an input to the next
//! stage, until finally arriving at the produced execution payloads.
//!
//! The [`ChannelProvider`] and [`BatchProvider`] stages are multiplexers whose active inner
//! stages depend on Holocene activation. [`BatchStream`] is always present in the composed stack,
//! but it only performs span-batch streaming after Holocene; before Holocene it passes batches
//! through unchanged.
//!
//! **Pre-Holocene effective stage order:**
//!
//! 1. L1 Traversal
//! 2. L1 Retrieval
//! 3. Frame Queue
//! 4. Channel Bank
//! 5. Channel Reader (Batch Decoding)
//! 6. Batch Stream (Pass-Through)
//! 7. Batch Queue
//! 8. Payload Attributes Derivation
//!
//! **Post-Holocene effective stage order:**
//!
//! 1. L1 Traversal
//! 2. L1 Retrieval
//! 3. Frame Queue
//! 4. Channel Assembler
//! 5. Channel Reader (Batch Decoding)
//! 6. Batch Stream (Span Batches)
//! 7. Batch Validator
//! 8. Payload Attributes Derivation

mod traversal;
pub use traversal::PollingTraversal;

mod l1_retrieval;
pub use l1_retrieval::{L1Retrieval, L1RetrievalProvider};

mod frame_queue;
pub use frame_queue::{FrameQueue, FrameQueueProvider};

mod channel;
pub use channel::{
    ChannelAssembler, ChannelBank, ChannelProvider, ChannelReader, ChannelReaderProvider,
    FJORD_MAX_CHANNEL_BANK_SIZE, MAX_CHANNEL_BANK_SIZE, NextFrameProvider,
};

mod batch;
pub use batch::{
    BatchProvider, BatchQueue, BatchStream, BatchStreamProvider, BatchValidator, NextBatchProvider,
};

mod attributes_queue;
pub use attributes_queue::AttributesQueue;
