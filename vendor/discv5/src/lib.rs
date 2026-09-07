#![deny(rustdoc::broken_intra_doc_links)]
//! An implementation of [Discovery V5](https://github.com/ethereum/devp2p/blob/master/discv5/discv5.md).
//!
//! # Overview
//!
//! Discovery v5 is a protocol designed for encrypted peer discovery and topic advertisement. Each
//! peer/node on the network is identified via it's ENR ([Ethereum Name
//! Record](https://eips.ethereum.org/EIPS/eip-778)), which is essentially a signed key-value store
//! containing the node's public key and optionally IP address and port.
//!
//! Discv5 employs a kademlia-like routing table to store and manage discovered peers. The protocol
//! allows for external IP discovery in NAT environments through regular PING/PONG's with
//! discovered nodes. Nodes return the external IP address that they have received and a simple
//! majority is chosen as our external IP address. If an external IP address is updated, this is
//! produced as an event.
//!
//! For a simple CLI discovery service see [discv5-cli](https://github.com/AgeManning/discv5-cli)
//!
//! This protocol is split into four main layers:
//!
//! - [`socket`]: Responsible for opening the underlying UDP socket. It creates individual tasks
//!   for sending/encoding and receiving/decoding packets from the UDP socket.
//! - [`handler`]: The protocol's communication is encrypted with `AES_GCM`. All node communication
//!   undergoes a handshake, which results in a `Session`. These are established when needed and get
//!   dropped after a timeout. The creation and maintenance of sessions between nodes and the
//!   encryption/decryption of packets from the socket is realised by the [`handler::Handler`] struct
//!   runnning in its own task.
//! - [`service`]: Contains the protocol-level logic. The [`service::Service`] manages the routing
//!   table of known ENR's, and performs parallel queries for peer discovery. It also runs in it's
//!   own task.
//! - [`Discv5`]: The application level. Manages the user-facing API. It starts/stops the underlying
//!   tasks, allows initiating queries and obtain metrics about the underlying server.
//!
//! ## Event Stream
//!
//! The [`Discv5`] struct provides access to an event-stream which allows the user to listen to
//! [`Event`] that get generated from the underlying server. The stream can be obtained from the
//! [`Discv5::event_stream`] function.
//!
//! ## Runtimes
//!
//! Discv5 requires a tokio runtime with timing and io enabled. An explicit runtime can be given
//! via the configuration. See the [`ConfigBuilder`] for further details. Such a runtime must
//! implement the [`Executor`] trait.
//!
//! If an explicit runtime is not provided via the configuration parameters, it is assumed that a
//! tokio runtime is present when creating the [`Discv5`] struct. The struct will use the existing
//! runtime for spawning the underlying server tasks. If a runtime is not present, the creation of
//! the [`Discv5`] struct will panic.
//!
//! # Usage
//!
//! A simple example of creating this service is as follows:
//!
//! ```rust
//!    use discv5::{enr, enr::{CombinedKey, Enr, NodeId}, TokioExecutor, Discv5, ConfigBuilder};
//!    use discv5::socket::ListenConfig;
//!    use std::net::{Ipv4Addr, SocketAddr};
//!
//!    // construct a local ENR
//!    let enr_key = CombinedKey::generate_secp256k1();
//!    let enr = enr::Enr::empty(&enr_key).unwrap();
//!
//!    // build the tokio executor
//!    let mut runtime = tokio::runtime::Builder::new_multi_thread()
//!        .thread_name("Discv5-example")
//!        .enable_all()
//!        .build()
//!        .unwrap();
//!
//!    // configuration for the sockets to listen on
//!    let listen_config = ListenConfig::Ipv4 {
//!        ip: Ipv4Addr::UNSPECIFIED,
//!        port: 9000,
//!    };
//!
//!    // default configuration
//!    let config = ConfigBuilder::new(listen_config).build();
//!
//!    // construct the discv5 server
//!    let mut discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();
//!
//!    // In order to bootstrap the routing table an external ENR should be added
//!    // This can be done via add_enr. I.e.:
//!    // discv5.add_enr(<ENR>)
//!
//!    // start the discv5 server
//!    runtime.block_on(discv5.start());
//!
//!    // run a find_node query
//!    runtime.block_on(async {
//!       let found_nodes = discv5.find_node(NodeId::random()).await.unwrap();
//!       println!("Found nodes: {:?}", found_nodes);
//!    });
//! ```

mod config;
mod discv5;
mod error;
mod executor;
pub mod handler;
mod ipmode;
pub mod kbucket;
mod lru_time_cache;
pub mod metrics;
mod node_info;
pub mod packet;
pub mod permit_ban;
mod query_pool;
pub mod rpc;
pub mod service;
pub mod socket;

#[macro_use]
extern crate lazy_static;

pub type Enr = enr::Enr<enr::CombinedKey>;

pub use config::{Config, ConfigBuilder};
// Re-export the ENR crate
pub use enr;
pub use error::{Error, QueryError, RequestError, ResponseError};
pub use executor::{Executor, TokioExecutor};
pub use ipmode::IpMode;
pub use kbucket::{ConnectionDirection, ConnectionState, Key};
// Re-export libp2p-identity and multiaddr
#[cfg(feature = "libp2p")]
pub use libp2p_identity;
#[cfg(feature = "libp2p")]
pub use multiaddr;
pub use packet::ProtocolIdentity;
pub use permit_ban::PermitBanList;
pub use service::TalkRequest;
pub use socket::{ListenConfig, RateLimiter, RateLimiterBuilder};

pub use crate::discv5::{Discv5, Event};
