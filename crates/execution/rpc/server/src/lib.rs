#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod server;
pub use server::{
    RpcModuleConfig, RpcRegistryInner, RpcServerConfig, RpcServerHandle, TransportRpcModuleConfig,
    TransportRpcModules,
};

mod config;
pub use config::RpcConfig;

mod cors;
pub use cors::CorsDomainError;

mod error;
pub use error::{RpcError, ServerKind, WsHttpSamePortError};

mod eth;
pub use eth::EthHandlers;

mod middleware;
pub use middleware::*;

mod metrics;
pub use metrics::{MeteredBatchRequestsFuture, MeteredRequestFuture, RpcRequestMetricsService};

mod rate_limiter;
pub use rate_limiter::*;

pub use jsonrpsee::server::ServerBuilder;
mod namespace;
pub use namespace::RpcNamespace;
pub use tower::layer::util::{Identity, Stack};
