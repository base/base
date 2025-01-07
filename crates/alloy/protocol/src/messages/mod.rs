//! Message types for OP Stack interoperability.

mod events;
pub use events::{ExecutingMessage, MessageIdentifier, MessagePayload};

mod safety;
pub use safety::SafetyLevel;
