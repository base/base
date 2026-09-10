#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate self as base_execution_txpool;

pub use imbl::OrdMap;

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
pub use best::MergeBestTransactions;

mod validity;
pub use validity::{
    DEFAULT_MAX_VALIDITY_PREDICATES, PredicateContext, TransactionValidity, ValidityOperator,
    ValidityPredicate, ValidityPredicateError,
};

mod block_expiry;
pub use block_expiry::BlockExpiryIndex;

mod transaction;
pub use transaction::{BasePooledTransaction, unix_time_millis};

mod base_ordering;
pub use base_ordering::{BaseOrdering, BasePriority, BestTransactionPriority, UnifiedTipPriority};

mod parking;
pub use parking::{
    BestTransactionLane, BestTransactionLaneState, ParkableBestTransactions,
    ParkableTransactionPool, ParkedBestTransactions,
};

mod base_pool;
pub use base_pool::{AccountStateDiff, BaseTransactionPool};

mod state_diff_maintain;
pub use state_diff_maintain::{
    InvalidationCause, StateDiffInvalidation, maintain_state_diff_invalidation,
};

mod pool_error_label;
pub use pool_error_label::PoolRejectionLabel;

mod wire;
pub use wire::{ExtensionError, ValidatedTransaction};

mod two_d_nonce_pool;
pub use two_d_nonce_pool::{BestTwoDTransactions, InsertOutcome, PruneMinedOutcome, TwoDNoncePool};

mod base_metrics;
pub use base_metrics::{GuardMetrics, ValidatorMetrics, ValidityPoolMetrics};

mod estimated_da_size;
pub use estimated_da_size::*;

mod batcher;
pub use batcher::*;

mod blobstore;
pub use blobstore::*;

mod config;
pub use config::*;

mod core_pool;
pub use core_pool::*;

mod error;
pub use error::*;

mod identifier;
pub use identifier::*;

mod maintain;
pub use maintain::*;

mod metrics;
pub use metrics::*;

mod noop;
pub use noop::*;

mod ordering;
pub use ordering::*;

mod pool;
pub use pool::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod traits;
pub use traits::*;

mod validate;
use aquamarine as _;
pub use pool::BestTransactions as PendingBestTransactions;
pub use traits::BestTransactions;
pub use validate::*;

mod payload_transactions;
pub use payload_transactions::{
    BestPayloadTransactions, NoopPayloadTransactions, PayloadTransactions,
    PayloadTransactionsChain, PayloadTransactionsFixed,
};

mod forwarding;
pub use forwarding::{
    DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS, DEFAULT_RESEND_AFTER_MS, ForwardRequest,
    ForwardingSetupError, InsertValidatedTransaction, ShutdownReport, TxForwardingConfig,
    TxForwardingHandle, TxForwardingService,
};

mod transaction_tracing;
pub use transaction_tracing::{
    EventLog, Metrics as TxpoolTracingMetrics, NonceSlot, NonceSummary, Pool as TracedPool,
    Tracker as TransactionTracker, TxEvent, TxpoolConfig as TransactionTracingConfig,
    tracex_subscription,
};

/// The production pool validates through the Base task executor.
#[cfg(not(any(test, feature = "test-utils")))]
pub type PoolValidator = TransactionValidationTaskExecutor;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::PoolValidator;
