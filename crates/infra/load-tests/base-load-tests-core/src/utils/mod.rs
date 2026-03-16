//! Utility types, error handling, and tracing setup.

mod errors;
pub use base_cli_utils::init_test_tracing as init_tracing;
pub use errors::{BaselineError, Result};
