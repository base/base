#![allow(clippy::needless_doctest_main)]
#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
extern crate tracing;

use revmc_backend::{eyre, *};
use revmc_context::*;

mod bytecode;
pub use bytecode::*;

mod compiler;
pub use compiler::{CompileTimings, EvmCompiler, EvmCompilerInput};

mod linker;
pub use linker::{Linker, shared_library_path};

/// Generic `revm` JIT EVM from `revmc-context`.
pub mod simple_revm_evm {
    pub use revmc_context::JitEvm;
}

/// ABI version of compiled artifacts. Bump when the calling convention changes.
pub const ABI_VERSION: u32 = 0;

/// Internal tests and testing utilities. Not public API.
#[cfg(any(test, feature = "__fuzzing"))]
pub mod tests;

type FxHashMap<K, V> = alloy_primitives::map::HashMap<K, V, alloy_primitives::map::FxBuildHasher>;

/// Enable for `cargo asm -p revmc --lib`.
#[cfg(any())]
pub fn generate_all_assembly() -> EvmCompiler<revmc_llvm::EvmLlvmBackend> {
    let mut compiler = EvmCompiler::new_llvm(false).unwrap();
    let _ = compiler.jit(None, &[], revm_primitives::hardfork::SpecId::LONDON).unwrap();
    unsafe { compiler.clear().unwrap() };
    compiler
}
