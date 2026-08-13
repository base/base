#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod backend;
pub use backend::{
    BoundlessRegistrationBackend, BoundlessRegistrationBackendConfig, DEFAULT_MAX_TX_RETRIES,
    DEFAULT_TX_RETRY_DELAY_SECS, NoopRegistrationBackend,
};

mod cert_manager;
pub use cert_manager::CertManager;

mod crl;
pub use crl::{CertCrlInfo, CrlError, check_chain_against_crls};

mod discovery;
pub use discovery::AwsTargetGroupDiscovery;

mod driver;
pub use driver::{
    DEFAULT_MAX_CONCURRENCY, DiscoveryResolution, DriverConfig, INSTANCE_CACHE_TTL_CYCLES,
    RegisterableSigner, RegistrationDriver,
};

mod error;
pub use error::{RegistrarError, Result};

mod metrics;
pub use metrics::RegistrarMetrics;

mod prover;
pub use prover::ProverClient;

mod signer_manager;
pub use signer_manager::{
    AttestationNonce, PendingRegistration, ProofTaskSet, SignerManager, SignerManagerConfig,
};

mod service;
pub use service::RegistrarConfig;

mod traits;
pub use traits::{EnclaveEndpointClient, InstanceDiscovery, RegistrationBackend};

mod types;
pub use types::{InstanceHealthStatus, ProverInstance};

#[cfg(test)]
pub mod test_utils;
