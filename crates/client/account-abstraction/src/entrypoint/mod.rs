//! EntryPoint Version Management
//!
//! This module provides abstractions for working with different EntryPoint versions.
//! Each version may have different:
//! - UserOperation formats
//! - Simulation methods
//! - Gas calculation logic
//! - PreVerificationGas requirements

mod version;

pub use version::{
    EntryPointVersion, EntryPointVersionResolver, SimulationCallData, SimulationResult,
};

use alloy_primitives::Address;

use crate::contracts::{
    ENTRYPOINT_V06_ADDRESS, ENTRYPOINT_V07_ADDRESS, ENTRYPOINT_V08_ADDRESS, ENTRYPOINT_V09_ADDRESS,
};

/// Get the EntryPointVersion for a given address
pub fn get_entrypoint_version(address: Address) -> Option<EntryPointVersion> {
    match address {
        addr if addr == ENTRYPOINT_V06_ADDRESS => Some(EntryPointVersion::V06),
        addr if addr == ENTRYPOINT_V07_ADDRESS => Some(EntryPointVersion::V07),
        addr if addr == ENTRYPOINT_V08_ADDRESS => Some(EntryPointVersion::V08),
        addr if addr == ENTRYPOINT_V09_ADDRESS => Some(EntryPointVersion::V09),
        _ => None,
    }
}

/// Check if an address is a supported EntryPoint
pub fn is_supported_entrypoint(address: Address) -> bool {
    get_entrypoint_version(address).is_some()
}

/// Get all supported EntryPoint addresses
pub fn supported_entrypoints() -> Vec<Address> {
    vec![
        ENTRYPOINT_V06_ADDRESS,
        ENTRYPOINT_V07_ADDRESS,
        ENTRYPOINT_V08_ADDRESS,
        ENTRYPOINT_V09_ADDRESS,
    ]
}

