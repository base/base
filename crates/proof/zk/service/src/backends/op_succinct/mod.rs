//! OP Succinct proving backends.

mod cluster;
pub use cluster::ClusterBackend;

mod dry_run;
pub use dry_run::{
    DRY_RUN_METADATA_KEY, DryRunBackend, EXECUTION_STATS_METADATA_KEY, StoredExecutionStats,
};

mod mock;
pub use mock::MockBackend;

mod network;
pub use network::NetworkBackend;

mod provider;
pub use provider::{OpSuccinctProvider, WitnessParams};
