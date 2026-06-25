//! Configuration types for load tests and workloads.

mod workload;
pub use workload::WorkloadConfig;

mod parsing;

mod precompile;
pub use precompile::PrecompileTarget;

mod real_token;
pub use real_token::{RealTokenAcquisitionConfig, RealTokenPairTokenConfig, RealTokenSetupConfig};

mod test_config;
pub use test_config::{OsakaTarget, TestConfig, TxTypeConfig, WeightedTxType};
