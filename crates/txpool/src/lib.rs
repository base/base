#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod eip8130_invalidation;
pub use eip8130_invalidation::{
    Eip8130InvalidationIndex, InvalidationKey, compute_invalidation_keys,
    maintain_eip8130_invalidation, process_fal,
};

mod eip8130_pool;
pub use eip8130_pool::{
    AddOutcome, BestEip8130Transactions, Eip8130Pool, Eip8130PoolConfig, Eip8130PoolError,
    Eip8130SequenceId, Eip8130TxId, SharedEip8130Pool, ThroughputTier, TierCheckResult,
};

mod base_pool;
pub use base_pool::BaseTransactionPool;

mod best;
pub use best::MergedBestTransactions;

mod eip8130_validate;
pub use eip8130_validate::{
    CustomVerifierPolicy, DEFAULT_CUSTOM_VERIFIER_GAS_LIMIT, Eip8130ValidationError,
    Eip8130ValidationOutcome, MAX_AA_TX_ENCODED_BYTES, VerifierAdmissionPolicy, VerifierAllowlist,
    VerifierPurityCache, compute_account_tier, validate_eip8130_transaction,
};

mod validator;
pub use validator::{OpL1BlockInfo, OpTransactionValidator};

mod transaction;
pub use transaction::{
    BLOCK_TIME_SECS, BasePooledTransaction, BundleTransaction, Eip8130Metadata,
    MAX_BUNDLE_ADVANCE_BLOCKS, MAX_BUNDLE_ADVANCE_MILLIS, MAX_BUNDLE_ADVANCE_SECS, OpPooledTx,
    TimestampedTransaction, unix_time_millis,
};

mod ordering;
pub use ordering::{BaseOrdering, TimestampOrdering};

mod consumer;
pub use consumer::{Consumer, ConsumerConfig, ConsumerMetrics, RecentlySent, SpawnedConsumer};

mod forwarder;
pub use forwarder::{Forwarder, ForwarderConfig, ForwarderMetrics, SpawnedForwarder};

mod builder;
pub use builder::{BuilderApiImpl, BuilderApiMetrics, BuilderApiServer};

mod bundle;
pub use bundle::{
    SendBundleApiImpl, SendBundleApiServer, SendBundleRequest, maintain_bundle_transactions,
};

mod wire;
pub use wire::{Eip8130WireMetadata, ValidatedTransaction};

pub mod estimated_da_size;

use reth_transaction_pool::{
    EthPoolTransaction, Pool, TransactionPool, TransactionValidationTaskExecutor,
};

/// The raw Reth pool type, before wrapping with [`BaseTransactionPool`].
pub type RawOpPool<Client, S, Evm, T = BasePooledTransaction, O = BaseOrdering<T>> =
    Pool<TransactionValidationTaskExecutor<OpTransactionValidator<Client, T, Evm>>, O, S>;

/// Type alias for the default Base transaction pool.
///
/// Wraps a raw Reth [`Pool`] with the EIP-8130 2D nonce side-pool so that
/// all [`TransactionPool`] methods transparently cover both standard and
/// 2D-nonce AA transactions.
pub type OpTransactionPool<Client, S, Evm, T = BasePooledTransaction, O = BaseOrdering<T>> =
    BaseTransactionPool<RawOpPool<Client, S, Evm, T, O>, T>;

/// Trait that exposes the [`SharedEip8130Pool`] from a transaction pool.
///
/// Implemented for [`BaseTransactionPool`] (and by extension
/// [`OpTransactionPool`]) so that consumers (payload builder, consumer,
/// invalidation task) can retrieve the 2D nonce pool through a generic pool
/// reference without needing concrete type knowledge.
pub trait HasEip8130Pool {
    /// The pool transaction type.
    type Tx: EthPoolTransaction;
    /// Returns a shared reference to the EIP-8130 2D nonce pool.
    fn eip8130_pool(&self) -> SharedEip8130Pool<Self::Tx>;
}

impl<P, T> HasEip8130Pool for BaseTransactionPool<P, T>
where
    P: TransactionPool<Transaction = T>,
    T: EthPoolTransaction + Clone,
{
    type Tx = T;

    fn eip8130_pool(&self) -> SharedEip8130Pool<T> {
        self.eip8130_pool()
    }
}
