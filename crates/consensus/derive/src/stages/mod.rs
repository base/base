//! This module contains each stage of the derivation pipeline.
//!
//! It offers a high-level API to functionally apply each stage's output as an input to the next
//! stage, until finally arriving at the produced execution payloads.
//!
//! **Effective stage order:**
//!
//! 1. L1 Traversal
//! 2. L1 Retrieval
//! 3. Frame Queue
//! 4. Channel Assembler
//! 5. Channel Reader (Batch Decoding)
//! 6. Batch Stream
//! 7. Batch Validator
//! 8. Payload Attributes Derivation

mod traversal;
pub use traversal::PollingTraversal;

mod l1_retrieval;
pub use l1_retrieval::{L1Retrieval, L1RetrievalProvider};

mod frame_queue;
pub use frame_queue::{FrameQueue, FrameQueueProvider};

mod channel;
pub use channel::{ChannelAssembler, ChannelReader, ChannelReaderProvider, NextFrameProvider};

mod batch;
pub use batch::{BatchStream, BatchStreamProvider, BatchValidator, NextBatchProvider};

mod attributes_queue;
pub use attributes_queue::AttributesQueue;
