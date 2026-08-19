#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cert_manager;
pub use cert_manager::CertManager;

mod crl;
pub use crl::{CertCrlInfo, CrlError, check_chain_against_crls};

mod discovery;
pub use discovery::{AwsTargetGroupDiscovery, parse_target_group_arns};

mod driver;
pub use driver::{
    DEFAULT_MAX_CONCURRENCY, DiscoveryResolution, DriverConfig, INSTANCE_CACHE_TTL_CYCLES,
    RegisterableSigner, RegistrationDriver,
};

mod error;
pub use error::{HintError, HintResult, PlannerError, PlannerResult, RegistrarError, Result};

mod hints;
pub use hints::P384Hints;

mod metrics;
pub use metrics::RegistrarMetrics;

mod cbor;
pub use cbor::{CborItem, NitroCose, ParsedCoseSign1};

mod planner;
pub use planner::{AttestationPlanner, CertManagerKeys, PINNED_ROOT_CERT_HASH};

mod prover;
pub use prover::ProverClient;

mod signer_manager;
pub use signer_manager::{
    CacheTxAttempt, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS, PendingRegistration,
    ProofTaskSet, SignerManager, SignerManagerConfig,
};

mod service;
pub use service::RegistrarConfig;

mod traits;
pub use traits::{EnclaveEndpointClient, InstanceDiscovery};

mod types;
pub use types::{
    CertKind, CertPlan, InstanceHealthStatus, ProverInstance, RegistrationHints, RegistrationPlan,
};

#[cfg(test)]
pub mod test_utils;
