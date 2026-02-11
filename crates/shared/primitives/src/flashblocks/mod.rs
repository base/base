//! Flashblocks Primitive Types.
//!
//! Low-level primitive types for flashblocks. Provides the core data structures
//! for flashblock payloads, metadata, and delta encoding used in Base's
//! pre-confirmation infrastructure.

mod block;
pub use block::Flashblock;

mod metadata;
pub use metadata::Metadata;

mod error;
pub use error::FlashblockDecodeError;

mod payload;
pub use payload::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
