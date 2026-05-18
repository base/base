#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod metrics;
pub use metrics::Metrics;

mod beacon_client;
pub use beacon_client::{
    APIConfigResponse, APIGenesisResponse, BeaconClient, BeaconClientError, OnlineBeaconClient,
    ReducedConfigData, ReducedGenesisData,
};

mod blobs;
pub use blobs::{BlobWithCommitmentAndProof, BoxedBlob, OnlineBlobProvider};

mod chain_provider;
pub use chain_provider::{AlloyChainProvider, AlloyChainProviderError};

mod conf_depth;
pub use conf_depth::{ConfDepthProvider, L1HeadNumber};

mod l2_chain_provider;
pub use l2_chain_provider::{AlloyL2ChainProvider, AlloyL2ChainProviderError};

mod pipeline;
pub use pipeline::OnlinePipeline;
