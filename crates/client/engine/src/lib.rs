//! Implements custom engine validator that is optimized for validating canonical blocks
//! after flashblock validation.

#[macro_use]
extern crate tracing;

pub mod validator;
pub use validator::BaseEngineValidatorBuilder;
