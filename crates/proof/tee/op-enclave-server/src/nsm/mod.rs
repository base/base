//! NSM (Nitro Secure Module) interface.
//!
//! This module provides:
//! - [`NsmSession`]: Session management for NSM operations
//! - [`NsmRng`]: A cryptographically secure RNG backed by NSM

mod random;
mod session;

pub use random::NsmRng;
pub use session::NsmSession;
