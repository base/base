//! Shared building blocks for B-20 token operations.
//!
//! Token behavior itself lives in each variant's versioned `logic/vN` implementation. What stays
//! here is the part every version must agree on: the authorization guards, the built-in role
//! identifiers, and the EIP-2612 permit argument hashing.

mod guards;
pub use guards::B20Guards;

mod permit;
pub use permit::{Eip712Domain, PermitArgs};

mod roles;
pub use roles::B20TokenRole;
