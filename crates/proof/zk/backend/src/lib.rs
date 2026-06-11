#![doc = include_str!("../README.md")]

mod succinct;
pub use succinct::{
    DRY_RUN_SNARK_PREFIX, DryRunZkProver, L1HeadSource, MOCK_PROOF_BYTES, MOCK_SNARK_PREFIX,
    MockZkProver, OpSuccinctWitnessProvider, WitnessError, WitnessParams,
};
