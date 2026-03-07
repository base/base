#![allow(clippy::all)]
#![allow(missing_docs)]
#![allow(elided_lifetimes_in_paths)]
#![allow(dead_code)]
#![allow(unused)]
//! This lib re-exports the contract bindings.

#[cfg_attr(rustfmt, rustfmt_skip)]
mod codegen;

#[cfg_attr(rustfmt, rustfmt_skip)]
pub use codegen::*;
