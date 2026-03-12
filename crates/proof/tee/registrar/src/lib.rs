#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{BoundlessConfig, RegistrarConfig, RemoteSignerConfig, SigningConfig};

mod error;
pub use error::{RegistrarError, Result};

mod traits;
pub use traits::{AttestationProofProvider, InstanceDiscovery};

mod types;
pub use types::{
    AttestationProof, AttestationResponse, InstanceHealthStatus, ProverInstance, RegisteredSigner,
};
