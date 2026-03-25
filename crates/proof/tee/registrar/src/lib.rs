#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{AwsDiscoveryConfig, BoundlessConfig, ProvingConfig, RegistrarConfig};

mod discovery;
pub use discovery::AwsTargetGroupDiscovery;

mod driver;
pub use driver::{DriverConfig, RegistrationDriver};

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
