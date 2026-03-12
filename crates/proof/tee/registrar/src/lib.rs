#![doc = include_str!("../README.md")]

mod config;
pub use config::{RegistrarConfig, RemoteSignerConfig, SigningConfig};

mod error;
pub use error::{RegistrarError, Result};

mod traits;
pub use traits::{AttestationProofProvider, InstanceDiscovery};

mod types;
pub use types::{
    AttestationProof, AttestationResponse, InstanceHealthStatus, ProverInstance, RegisteredSigner,
};
