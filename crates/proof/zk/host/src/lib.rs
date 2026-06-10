#![doc = include_str!("../README.md")]

mod prover;
pub use prover::{
    UnimplementedZkProver, ZkProofRequestKind, ZkProver, ZkProverError, ZkSessionState,
};
