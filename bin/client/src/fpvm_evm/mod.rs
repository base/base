//! Custom [`EvmFactory`] for the fault proof virtual machine's EVM.
//!
//! [`EvmFactory`]: alloy_evm::EvmFactory

mod precompiles;
pub use precompiles::OpFpvmPrecompiles;

mod factory;
pub use factory::FpvmOpEvmFactory;
