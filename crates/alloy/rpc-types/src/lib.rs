#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

mod config;
pub use config::{Genesis, RollupConfig, SystemConfig};

mod genesis;
pub use genesis::{OpBaseFeeInfo, OpChainInfo, OpGenesisInfo};

mod net;
pub use net::{
    Connectedness, Direction, GossipScores, PeerDump, PeerInfo, PeerScores, PeerStats,
    ReqRespScores, TopicScores,
};

mod output;
pub use output::OutputResponse;

mod receipt;
pub use receipt::{L1BlockInfo, OpTransactionReceipt, OpTransactionReceiptFields};

mod safe_head;
pub use safe_head::SafeHeadResponse;

mod sync;
pub use sync::{L1BlockRef, L2BlockRef, SyncStatus};

mod transaction;
pub use transaction::{
    OpTransactionFields, OpTransactionRequest, Transaction, TransactionConversionError,
};
