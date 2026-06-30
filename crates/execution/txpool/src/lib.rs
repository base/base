#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod validator;
pub use validator::{BaseL1BlockInfo, BaseTransactionValidator, BaseTxPoolError};

mod best;

mod transaction;
pub use transaction::{
    BLOCK_TIME_SECS, BasePooledTransaction, BasePooledTx, BundleTransaction,
    MAX_BUNDLE_ADVANCE_BLOCKS, MAX_BUNDLE_ADVANCE_MILLIS, MAX_BUNDLE_ADVANCE_SECS,
    TimestampedTransaction, unix_time_millis,
};

mod ordering;
pub use ordering::{BaseOrdering, TimestampOrdering};

mod consumer;
pub use consumer::{Consumer, ConsumerConfig, ConsumerMetrics, RecentlySent, SpawnedConsumer};

mod forwarder;
pub use forwarder::{Forwarder, ForwarderConfig, ForwarderMetrics, SpawnedForwarder};

mod pool;
pub use pool::{AccountStateDiff, BaseTransactionPool};

mod pool_error_label;
pub use pool_error_label::PoolRejectionLabel;

mod builder;
pub use builder::{BuilderApiImpl, BuilderApiMetrics, BuilderApiServer};

mod bundle;
pub use bundle::{
    BundleApiMetrics, SendBundleApiImpl, SendBundleApiServer, SendBundleRequest,
    maintain_bundle_transactions,
};

mod wire;
pub use wire::ValidatedTransaction;

mod two_d_nonce_pool;

mod invalidation;
pub use invalidation::{InvalidationIndex, InvalidationKey, WatchSet};

mod limits;
pub use limits::{InflightCounters, PayerBook};

mod guard;
pub use guard::{
    Admission, DEFAULT_PAYER_LIMIT, DEFAULT_SENDER_LIMIT, GuardLimits, LimitClass, LimitRejection,
    MempoolGuard,
};

pub mod estimated_da_size;
