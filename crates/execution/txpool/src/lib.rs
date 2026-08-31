#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod guard;
pub use guard::{
    Admission, AdmissionRecord, DEFAULT_PAYMENT_LIMIT, DEFAULT_SIGNATURE_LIMIT, GuardLimits,
    LimitClass, LimitRejection, MempoolGuard,
};

mod invalidation;
pub use invalidation::{InvalidationIndex, InvalidationKey, WatchSet};

mod manifest;
pub use manifest::{ConfigSlot, ManifestStale, WatchManifest};

mod limits;
pub use limits::{InflightCounters, PayerBook};

mod validator;
pub use validator::{BaseL1BlockInfo, BaseTransactionValidator, BaseTxPoolError, LimitClassCache};

mod best;

mod validity;
pub use validity::{
    DEFAULT_MAX_VALIDITY_PREDICATES, FIRST_POOL_FLASHBLOCK_INDEX, PredicateContext,
    TransactionValidity, ValidityOperator, ValidityPredicate, ValidityPredicateError,
};

mod block_expiry;
pub use block_expiry::BlockExpiryIndex;

mod transaction;
pub use transaction::{
    BasePooledTransaction, BasePooledTx, BaseTransactionIdentity, BaseTransactionLane,
    TimestampedTransaction, unix_time_millis,
};

mod ordering;
pub use ordering::{
    BaseOrdering, BasePriority, BestTransactionPriority, TimestampOrdering, UnifiedTipOrdering,
    UnifiedTipPriority,
};

mod parking;
pub use parking::{
    BestTransactionLaneState, ParkableBestTransactions, ParkableTransactionPool,
    ParkedBestTransactions,
};

mod pool;
pub use pool::{AccountStateDiff, BaseTransactionPool};

mod state_diff_maintain;
pub use state_diff_maintain::{
    InvalidationCause, StateDiffInvalidation, maintain_state_diff_invalidation,
};

mod pool_error_label;
pub use pool_error_label::PoolRejectionLabel;

mod builder;
pub use builder::{BuilderApiImpl, BuilderApiMetrics, BuilderApiServer};

mod wire;
pub use wire::{
    ExtensionError, NoExtensions, ValidatedTransaction, ValidatedTransactionExtensions,
};

mod two_d_nonce_pool;

mod metrics;
pub use metrics::{GuardMetrics, ValidatorMetrics, ValidityPoolMetrics};

pub mod estimated_da_size;
