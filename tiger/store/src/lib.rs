#![doc = include_str!("../README.md")]

#[allow(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    unused_imports,
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod mdbx_sys;

mod mdbx;
pub use mdbx::{Cursor, Dbi, Entry, Env, EnvInner, EnvOptions, MdbxError, Ro, Rw, Txn};

mod fault;
pub use fault::{Fault, FaultConfig, FaultMode};
