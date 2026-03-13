#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{BoundlessConfig, RegistrarConfig, RemoteSignerConfig, SigningConfig};

mod discovery;
pub use discovery::AwsTargetGroupDiscovery;

mod error;
pub use error::{RegistrarError, Result};

mod registry;
pub use registry::{RegistryClient, RegistryContractClient};

mod traits;
pub use traits::{AttestationProofProvider, InstanceDiscovery};

mod types;
pub use types::{
    AttestationProof, AttestationResponse, InstanceHealthStatus, ProverInstance, RegisteredSigner,
};
