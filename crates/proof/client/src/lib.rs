#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[macro_use]
extern crate tracing;

mod error;
pub use error::FaultProofProgramError;

mod epilogue;
pub use epilogue::Epilogue;

mod prologue;
pub use prologue::{BootInfo, HintType, OracleProviderError, Prologue};

mod driver;
pub use driver::FaultProofDriver;
