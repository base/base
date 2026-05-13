#![doc = include_str!("../README.md")]

pub mod abi;
pub mod counter;

pub use counter::{COUNTER_ADDRESS, Counter, dispatch};
