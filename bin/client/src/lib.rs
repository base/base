#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod fpvm_evm;
pub use fpvm_evm::{FpvmOpEvmFactory, OpFpvmPrecompiles};

mod single;
pub use single::{FaultProofProgramError, fetch_safe_head_hash, run};
