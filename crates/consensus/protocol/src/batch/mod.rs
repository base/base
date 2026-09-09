//! Singular batch encoding, channel reading, and validation.

mod reader;
pub use reader::{BatchReader, BatchReaderError, DecompressionError};
mod tx;
pub use tx::BatchTransaction;
mod errors;
pub use errors::BatchDecodingError;
mod validity;
pub use validity::{BatchDropReason, BatchValidity};
mod single;
pub use single::SingleBatch;
mod traits;
mod wire;
pub use traits::BatchValidationProvider;
