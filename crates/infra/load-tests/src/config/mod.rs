//! Configuration types for load tests and workloads.

mod workload;
pub use workload::WorkloadConfig;

mod scenarios;
pub use scenarios::{ScenarioConfig, ScenariosRegistry};

mod test_config;
pub use test_config::{PrecompileTarget, TestConfig, TxTypeConfig, WeightedTxType};
