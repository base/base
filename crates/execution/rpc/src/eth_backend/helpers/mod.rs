//! The entire implementation of the namespace is quite large, hence it is divided across several
//! files.

mod signer;

pub use signer::*;
mod sync_listener;
pub use sync_listener::*;
mod types;
