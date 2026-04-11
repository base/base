#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod precompiles;
pub use precompiles::FpvmPrecompiles;

mod factory;
pub use factory::FpvmEvmFactory;
