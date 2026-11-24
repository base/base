#![doc = include_str!("../README.md")]

mod adapter;
pub use adapter::Adapter;

mod witness;
pub use witness::{WitnessDb, WitnessError, WitnessState};
