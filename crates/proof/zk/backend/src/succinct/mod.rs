//! SP1 (Succinct) ZK proving backends.
//!
//! Each backend implements [`base_proof_zk_host::ZkProver`] for a different SP1
//! execution target.

mod mock;
pub use mock::{MOCK_PROOF_BYTES, MOCK_SNARK_PREFIX, MockZkProver};
