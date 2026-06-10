#![doc = include_str!("../README.md")]

mod succinct;
pub use succinct::{MOCK_PROOF_BYTES, MOCK_SNARK_PREFIX, MockZkProver};
