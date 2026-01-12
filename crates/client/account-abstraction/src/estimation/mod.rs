//! Gas Estimation for UserOperations
//!
//! This module provides gas estimation functionality for EIP-4337 UserOperations.
//! It includes:
//! - Binary search for optimal gas limits
//! - PreVerificationGas calculation (extensible by chain)
//! - Version-specific simulation handling

mod gas_estimator;
mod pre_verification_gas;

pub use gas_estimator::{
    GasEstimationConfig, GasEstimationError, GasEstimationResult, GasEstimator,
    SimulationCallResult, SimulationProvider,
};
// Re-export trace types from alloy for convenience
pub use alloy_rpc_types_trace::geth::{GethDebugTracingCallOptions, GethTrace};
pub use pre_verification_gas::{
    DefaultPreVerificationGasCalculator, OptimismPreVerificationGasCalculator,
    PreVerificationGasCalculator, PreVerificationGasOracle, RethPreVerificationGasCalculator,
};

