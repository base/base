#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod config;
#[cfg(feature = "client")]
pub use config::BaseEthConfigApiClient;
pub use config::{BaseEthConfigApiServer, BaseEthConfigHandler};

mod debug;
#[cfg(test)]
pub use debug::DebugApiOverrideClient;
pub use debug::{DebugApiExt, DebugApiExtInner, DebugApiOverrideServer, ProofsSyncStatus};

mod engine;
#[cfg(feature = "client")]
pub use engine::BaseEngineApiClient;
pub use engine::{BaseEngineApi, BaseEngineApiServer, ENGINE_CAPABILITIES};

mod error;
pub use error::{BaseEthApiError, BaseInvalidTransactionError, SequencerClientError};

mod eth;
#[cfg(test)]
pub use eth::EthApiOverrideClient;
pub use eth::{
    BaseEthApi, BaseEthApiBuilder, BaseEthApiInner, BaseReceiptBuilder, BaseReceiptConverter,
    BaseRpcConvert, BaseTxInfoMapper, EthApiExt, EthApiNodeBackend, EthApiOverrideServer,
    MAX_PROOF_KEYS, ProofKeyLimit, ReceiptFieldsBuilder,
};

mod metrics;
pub use metrics::{DebugApiExtMetrics, DebugApis, EthApiExtMetrics, SequencerMetrics};

mod miner;
#[cfg(feature = "client")]
pub use miner::MinerApiExtClient;
pub use miner::{BaseMinerExtApi, MinerApiExtServer};

mod sequencer;
pub use sequencer::{Error as SequencerError, SequencerClient, SequencerClientInner};

mod state;
pub use state::BaseStateProviderFactory;

mod witness;
pub use witness::BaseDebugWitnessApi;
