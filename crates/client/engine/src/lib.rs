//! Implements custom engine validator that is optimized for validating canonical blocks
//! after flashblock validation.

pub mod validator;
pub use validator::{BaseEngineValidator, BaseEngineValidatorBuilder};
