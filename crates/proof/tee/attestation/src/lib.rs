#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod types;
pub use types::{
    BoxError, Result, TeeAttestationKind, TeeAttestationProof, TeeAttestationProofProvider,
};
