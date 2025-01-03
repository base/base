//! Message types for OP Stack interoperability.

mod events;
pub use events::{MessageIdentifier, MessagePayload};

mod safety;
pub use safety::SafetyLevel;
