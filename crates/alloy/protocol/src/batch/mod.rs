//! Contains batch types.

mod r#type;
pub use r#type::*;

mod errors;
pub use errors::{SpanBatchError, SpanDecodingError};

mod validity;
pub use validity::BatchValidity;

mod single;
pub use single::SingleBatch;

mod traits;
pub use traits::BatchValidationProvider;
