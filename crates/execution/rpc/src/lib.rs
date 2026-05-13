#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod config;
mod debug;
mod engine;
mod error;
mod eth;
mod metrics;
mod miner;
mod sequencer;
mod state;
mod witness;

#[cfg(feature = "client")]
pub use config::BaseEthConfigApiClient;
pub use config::{BaseEthConfigApiServer, BaseEthConfigHandler};
#[cfg(test)]
pub use debug::DebugApiOverrideClient;
pub use debug::{DebugApiExt, DebugApiExtInner, DebugApiOverrideServer, ProofsSyncStatus};
#[cfg(feature = "client")]
pub use engine::BaseEngineApiClient;
pub use engine::{BaseEngineApi, BaseEngineApiServer, ENGINE_CAPABILITIES};
pub use error::{BaseEthApiError, BaseInvalidTransactionError, SequencerClientError};
#[cfg(test)]
pub use eth::EthApiOverrideClient;
pub use eth::{
    BaseEthApi, BaseEthApiBuilder, BaseEthApiInner, BaseReceiptBuilder, BaseReceiptConverter,
    BaseRpcConvert, BaseTxInfoMapper, EthApiExt, EthApiNodeBackend, EthApiOverrideServer,
    MAX_PROOF_KEYS, ProofKeyLimit, ReceiptFieldsBuilder,
};
pub use metrics::{DebugApiExtMetrics, DebugApis, EthApiExtMetrics, SequencerMetrics};
#[cfg(feature = "client")]
pub use miner::MinerApiExtClient;
pub use miner::{BaseMinerExtApi, MinerApiExtServer};
pub use sequencer::{Error as SequencerError, SequencerClient, SequencerClientInner};
pub use state::BaseStateProviderFactory;
pub use witness::BaseDebugWitnessApi;
