#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cert_manager;
pub use cert_manager::CertManager;

mod crl;
pub use crl::{CertCrlInfo, CrlError, check_chain_against_crls};

mod discovery;
pub use discovery::AwsTargetGroupDiscovery;

mod driver;
pub use driver::{
    DEFAULT_MAX_CONCURRENCY, DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, DiscoveryResolution,
    DriverConfig, RegisterableSigner, RegistrationDriver,
};

mod error;
pub use error::{RegistrarError, Result};

mod metrics;
pub use metrics::RegistrarMetrics;

mod prover;
pub use prover::ProverClient;

mod signer_manager;
pub use signer_manager::{
    DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS, PendingRegistration, ProofTaskSet,
    SignerManager,
};

mod service;
pub use service::RegistrarConfig;

mod traits;
pub use traits::{InstanceDiscovery, SignerClient};

mod types;
pub use types::{InstanceHealthStatus, ProverInstance, RegisteredSigner};

mod verifier;
pub use verifier::{NitroVerifierClient, NitroVerifierContractClient};

#[cfg(test)]
pub mod test_utils;
