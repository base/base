#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod error;
pub use error::FaultProofProgramError;

mod epilogue;
pub use epilogue::Epilogue;

mod prologue;
pub use prologue::{Prologue, BootInfo, CachingOracle, HintType, OracleProviderError};

mod driver;
pub use driver::FaultProofDriver;
