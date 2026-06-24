#![doc = include_str!("../README.md")]

mod error;
pub use error::ZkProofRequesterError;

mod request;
pub use request::Groth16RangeProofRequest;

mod requester;
pub use requester::{Groth16ProofRequestResponse, ZkProofRequester};
