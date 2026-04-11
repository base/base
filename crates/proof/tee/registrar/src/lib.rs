#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    AwsDiscoveryConfig, BoundlessConfig, CrlConfig, DEFAULT_MAX_ATTESTATION_AGE_SECS,
    DEFAULT_MAX_RECOVERY_ATTEMPTS, ProvingConfig, RegistrarConfig,
};

mod crl;
pub use crl::{
    CertCrlInfo, CrlError, DEFAULT_CRL_FETCH_TIMEOUT_SECS, RevokedCertInfo, build_crl_http_client,
    check_chain_against_crls,
};

mod discovery;
pub use discovery::AwsTargetGroupDiscovery;

mod driver;
pub use driver::{
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, DriverConfig, RegistrationDriver,
};

mod error;
pub use error::{RegistrarError, Result};

mod metrics;
pub use metrics::RegistrarMetrics;

mod prover;
pub use prover::ProverClient;

mod registry;
pub use registry::{RegistryClient, RegistryContractClient};

mod traits;
pub use traits::{InstanceDiscovery, SignerClient};

mod types;
pub use types::{InstanceHealthStatus, ProverInstance, RegisteredSigner};
