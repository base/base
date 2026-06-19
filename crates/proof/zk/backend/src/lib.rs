#![doc = include_str!("../README.md")]

mod succinct;
pub use succinct::{
    ClusterSessionId, ClusterZkProver, ClusterZkProverConfig, DRY_RUN_SNARK_PREFIX, DryRunZkProver,
    L1HeadSource, MOCK_PROOF_BYTES, MOCK_SNARK_PREFIX, MockZkProver, NetworkZkProver,
    NetworkZkProverConfig, OpSuccinctWitnessProvider, WitnessError, WitnessParams,
};
