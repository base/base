#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    AwsDiscoveryConfig, BoundlessConfig, RegistrarConfig, RemoteSignerConfig, SigningConfig,
};

mod discovery;
pub use discovery::AwsTargetGroupDiscovery;

mod error;
pub use error::{RegistrarError, Result};

mod prover;
pub use prover::ProverClient;

mod registry;
pub use registry::{RegistryClient, RegistryContractClient};

mod traits;
pub use traits::InstanceDiscovery;

mod types;
// Re-exported from `base-proof-tee-nitro-attestation-prover`. Previously
// defined in this crate; the prover crate is now the single source of truth.
pub use base_proof_tee_nitro_attestation_prover::{
    AttestationProof, AttestationProofProvider, BoundlessProver, DirectProver, ProverError,
};
pub use types::{AttestationResponse, InstanceHealthStatus, ProverInstance, RegisteredSigner};
