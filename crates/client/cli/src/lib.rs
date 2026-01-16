#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{ConfigError, L1ConfigFile, L2ConfigFile};

mod l1;
pub use l1::L1ClientArgs;

mod l2;
pub use l2::L2ClientArgs;

mod builder;
pub use builder::BuilderClientArgs;

mod rpc;
pub use rpc::RpcArgs;

mod sequencer;
pub use sequencer::SequencerArgs;

mod boost;
pub use base_jwt::{JwtError, JwtSecret, default_jwt_secret};
pub use boost::{FlashblocksFlags, FlashblocksWebsocketFlags, RollupBoostFlags};

pub mod signer;
pub use signer::{SignerArgs, SignerArgsParseError};

pub mod p2p;
pub use p2p::{P2PArgs, P2PConfigError};
