#![doc = include_str!("../README.md")]

mod provider_ext;
pub use provider_ext::{DebugProviderExt, OptimismRollupProviderExt};

mod types;
pub use types::{GenesisL2BlockRef, L1BlockId, L1BlockRef, L2BlockRef, OutputAtBlock, SyncStatus};

mod api;
pub use api::{
    AdminApiClient, BaseApiClient, BaseP2PApiClient, ConductorApiClient, DevEngineApiClient,
    HealthzApiClient, RollupNodeApiClient, WsClient,
};
#[cfg(feature = "server")]
pub use api::{
    AdminApiServer, BaseApiServer, BaseP2PApiServer, ConductorApiServer, DevEngineApiServer,
    HealthzApiServer, RollupNodeApiServer, WsServer,
};

mod sync_api;
pub use sync_api::SyncStatusApiClient;
#[cfg(feature = "server")]
pub use sync_api::SyncStatusApiServer;
