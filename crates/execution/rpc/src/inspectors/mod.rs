//! Revm inspector implementations.
//! revm [Inspector](base_execution_evm_runtime::Inspector) implementations, such as call tracers
//!
//! ## Feature Flags
//!
//! - `js-tracer`: Enables a JavaScript tracer implementation. This pulls in extra dependencies
//!   (such as `boa`, `tokio` and `serde_json`).

/// An inspector implementation for an EIP2930 Accesslist
pub mod access_list;

/// implementation of an opcode counter for the EVM.
pub mod opcode;

/// An inspector for recording traces
pub mod tracing;

/// An inspector for recording internal transfers.
pub mod transfer;

/// An inspector for tracking storage access.
pub mod storage;

pub use colorchoice::ColorChoice;
