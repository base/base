#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod config;
pub mod debug;
pub mod engine;
pub mod error;
pub mod eth;
pub mod metrics;
pub mod miner;
pub mod resource_metering;
pub mod sequencer;
pub mod state;
mod trace_middleware;
pub use trace_middleware::{
    InboundOtelContext, OtelHttpMiddleware, OtelHttpMiddlewareLayer, OtelRpcMiddleware,
    OtelRpcMiddlewareLayer,
};
pub mod witness;

pub use config::{BaseEthConfigApiServer, BaseEthConfigHandler};
#[cfg(feature = "client")]
pub use engine::BaseEngineApiClient;
pub use engine::{BaseEngineApi, BaseEngineApiServer, ENGINE_CAPABILITIES};
pub use error::{BaseEthApiError, BaseInvalidTransactionError, SequencerClientError};
pub use eth::{BaseEthApi, BaseEthApiBuilder, BaseReceiptBuilder, BaseTimeCache};
pub use metrics::{DebugApiExtMetrics, DebugApis, EthApiExtMetrics, SequencerMetrics};
#[cfg(feature = "client")]
pub use miner::MinerApiExtClient;
pub use miner::MinerApiExtServer;
#[cfg(feature = "client")]
pub use resource_metering::{ResourceMeteringApiClient, ResourceMeteringScheduleApiClient};
pub use resource_metering::{
    ResourceMeteringApiExt, ResourceMeteringApiServer, ResourceMeteringScheduleApiServer,
    ResourceMeteringScheduleExt,
};
pub use sequencer::{SequencerClient, SequencerClientInner};
