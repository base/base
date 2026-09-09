#![doc = include_str!("../README.md")]
#![deny(rustdoc::broken_intra_doc_links)]

#[macro_use]
extern crate lazy_static;

mod config;
pub use config::{Config, ConfigBuilder};

mod discv5;
pub use discv5::{Discv5, Event};

mod error;
pub use error::{Error, PacketError, QueryError, RequestError, ResponseError};

mod executor;
pub use executor::{Executor, ExecutorClone, TokioExecutor};

mod handler;
pub use handler::*;

mod ipmode;
pub use ipmode::{IpMode, to_ipv4_mapped};

mod kbucket;
pub use kbucket::Filter as KBucketFilter;
#[cfg(test)]
pub use kbucket::bucket_tests;
pub use kbucket::*;

mod lru_time_cache;
pub use lru_time_cache::LruTimeCache;

mod metrics;
pub use metrics::*;

mod node_info;
pub use node_info::{Enr, NodeAddress, NodeContact, NonContactable};

mod packet;
pub use packet::*;

mod permit_ban;
pub use permit_ban::PermitBanList;

mod query_pool;
pub use query_pool::{
    ClosestQueryPeer, ClosestQueryPeerState, FindNodeQuery, FindNodeQueryConfig, PredicateQuery,
    PredicateQueryConfig, PredicateQueryPeer, PredicateQueryPeerState, Query, QueryId, QueryPool,
    QueryPoolState, QueryResult, QueryState, TargetKey,
};

mod rpc;
pub use rpc::*;

mod service;
#[cfg(test)]
pub use service::test as service_tests;
pub use service::*;

mod socket;
pub use socket::Filter;
pub use socket::*;

pub use enr;
#[cfg(feature = "libp2p")]
pub use libp2p_identity;
#[cfg(feature = "libp2p")]
pub use multiaddr;
