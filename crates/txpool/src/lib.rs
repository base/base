#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod validator;
pub use validator::{OpL1BlockInfo, OpTransactionValidator};

mod transaction;
pub use transaction::{BasePooledTransaction, OpPooledTx, TimestampedTransaction};

mod ordering;
pub use ordering::{BaseOrdering, TimestampOrdering};

mod consumer;
pub use consumer::{Consumer, ConsumerConfig, ConsumerHandle, ConsumerMetrics, RecentlySent};

mod forwarder;
pub use forwarder::{Forwarder, ForwarderConfig, ForwarderHandle, ForwarderMetrics};

mod builder;
pub use builder::{BuilderApiImpl, BuilderApiMetrics, BuilderApiServer};

mod wire;
pub use wire::ValidTransaction;

pub mod estimated_da_size;

use reth_transaction_pool::{Pool, TransactionValidationTaskExecutor};

/// Type alias for default optimism transaction pool
pub type OpTransactionPool<Client, S, Evm, T = BasePooledTransaction, O = BaseOrdering<T>> =
    Pool<TransactionValidationTaskExecutor<OpTransactionValidator<Client, T, Evm>>, O, S>;
