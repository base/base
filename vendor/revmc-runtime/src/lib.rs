#![doc = include_str!("../README.md")]
#![allow(clippy::needless_doctest_main)]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
extern crate tracing;

pub use ::eyre;
use revmc_backend::OptimizationLevel;
pub use revmc_codegen::*;
use revmc_context::EvmCompilerFn;
#[cfg(feature = "llvm")]
use revmc_llvm as llvm;
#[cfg(feature = "llvm")]
use revmc_llvm::EvmLlvmBackend;

pub mod runtime;

pub mod revm_evm;

#[cfg(feature = "alloy-evm")]
pub mod alloy_evm;
