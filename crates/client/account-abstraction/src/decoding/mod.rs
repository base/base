//! Simulation Result Decoding
//!
//! This module provides clean decoding of simulation results and revert data
//! from EntryPoint contracts across all supported versions (v0.6, v0.7, v0.8).
//!
//! # Overview
//!
//! EntryPoint simulation functions (`simulateHandleOp`, `simulateValidation`) can
//! return results in different ways depending on the version:
//!
//! - **v0.6**: Always reverts. Success = `ExecutionResult` error, failure = `FailedOp`/etc.
//! - **v0.7+**: Returns `ExecutionResult` on success, reverts with `FailedOp`/etc. on failure.
//!
//! # Usage
//!
//! ```ignore
//! use crate::decoding::{decode_simulation_revert, SimulationRevertDecoded};
//!
//! let result = decode_simulation_revert(EntryPointVersion::V06, &revert_data);
//! match result {
//!     SimulationRevertDecoded::ExecutionResult(exec) => { /* success */ }
//!     SimulationRevertDecoded::ValidationRevert(revert) => { /* failure */ }
//!     SimulationRevertDecoded::Unknown(data) => { /* unknown error */ }
//! }
//! ```

mod types;
mod v06;
mod v07;

pub use types::*;
pub use v06::decode_v06_simulation_revert;
pub use v07::{decode_v07_simulation_revert, decode_validation_revert};

use crate::entrypoint::EntryPointVersion;
use alloy_primitives::Bytes;

/// Decode simulation revert data based on EntryPoint version
///
/// This is the main entry point for decoding simulation results.
/// It dispatches to version-specific decoders.
pub fn decode_simulation_revert(
    version: EntryPointVersion,
    revert_data: &Bytes,
) -> SimulationRevertDecoded {
    match version {
        EntryPointVersion::V06 => decode_v06_simulation_revert(revert_data),
        EntryPointVersion::V07 | EntryPointVersion::V08 => decode_v07_simulation_revert(revert_data),
    }
}

