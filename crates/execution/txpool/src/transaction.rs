use core::fmt::Debug;
use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};

use alloy_consensus::{BlobTransactionValidationError, Typed2718, transaction::Recovered};
use alloy_eips::{
    eip2718::{Encodable2718, WithEncoded},
    eip2930::AccessList,
    eip7594::BlobTransactionSidecarVariant,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use base_common_consensus::BaseTransactionSigned;
use c_kzg::KzgSettings;
use reth_primitives_traits::{InMemorySize, SignedTransaction};
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, EthPooledTransaction, PoolTransaction,
};

use crate::estimated_da_size::DataAvailabilitySized;

/// Assumed L2 block time in seconds, used to convert block-based bundle windows
/// to time-based bounds.
pub const BLOCK_TIME_SECS: u64 = 2;

/// Maximum allowed advance window for bundle parameters (seconds).
pub const MAX_BUNDLE_ADVANCE_SECS: u64 = 60;

/// Maximum allowed advance window for bundle parameters (milliseconds).
pub const MAX_BUNDLE_ADVANCE_MILLIS: u64 = MAX_BUNDLE_ADVANCE_SECS * 1000;

/// Maximum allowed advance window in blocks.
pub const MAX_BUNDLE_ADVANCE_BLOCKS: u64 = MAX_BUNDLE_ADVANCE_SECS / BLOCK_TIME_SECS;

/// Returns current time as milliseconds since Unix epoch.
pub fn unix_time_millis() -> u128 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => dur.as_millis(),
        Err(err) => {
            tracing::warn!(error = %err, "system clock before Unix epoch, using 0 as timestamp");
            0
        }
    }
}

/// Pool transaction for OP.
///
/// This type wraps the actual transaction and caches values that are frequently used by the pool.
/// For payload building this lazily tracks values that are required during payload building:
///  - Estimated compressed size of this transaction
#[derive(Debug, Clone, derive_more::Deref)]
pub struct BasePooledTransaction<
    Cons = BaseTransactionSigned,
    Pooled = base_common_consensus::BasePooledTransaction,
> {
    #[deref]
    inner: EthPooledTransaction<Cons>,
    /// The estimated size of this transaction, lazily computed.
    estimated_tx_compressed_size: OnceLock<u64>,
    /// The pooled transaction type.
    _pd: core::marker::PhantomData<Pooled>,
    /// Cached EIP-2718 encoded bytes of the transaction, lazily computed.
    encoded_2718: OnceLock<Bytes>,
    /// Timestamp (millis since Unix epoch) when this transaction was received.
    received_at: u128,
    /// Optional target block number from bundle submission.
    target_block_number: Option<u64>,
    /// Optional minimum timestamp (millis since Unix epoch) from bundle submission.
    /// The transaction should not be included before this time.
    min_timestamp: Option<u64>,
    /// Optional maximum timestamp (millis since Unix epoch) from bundle submission.
    /// The transaction should be evicted after this time.
    max_timestamp: Option<u64>,
}

impl<Cons: SignedTransaction, Pooled> BasePooledTransaction<Cons, Pooled> {
    /// Create new instance of [Self].
    pub fn new(transaction: Recovered<Cons>, encoded_length: usize) -> Self {
        Self {
            inner: EthPooledTransaction::new(transaction, encoded_length),
            estimated_tx_compressed_size: Default::default(),
            _pd: core::marker::PhantomData,
            encoded_2718: Default::default(),
            received_at: unix_time_millis(),
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
        }
    }

    /// Create new instance with an explicit `received_at` timestamp (millis since Unix epoch).
    ///
    /// Primarily for testing.
    pub fn new_with_received_at(
        transaction: Recovered<Cons>,
        encoded_length: usize,
        received_at: u128,
    ) -> Self {
        Self {
            inner: EthPooledTransaction::new(transaction, encoded_length),
            estimated_tx_compressed_size: Default::default(),
            _pd: core::marker::PhantomData,
            encoded_2718: Default::default(),
            received_at,
            target_block_number: None,
            min_timestamp: None,
            max_timestamp: None,
        }
    }

    /// Sets bundle metadata on this transaction, returning the modified instance.
    pub const fn with_bundle_metadata(
        mut self,
        target_block_number: Option<u64>,
        min_timestamp: Option<u64>,
        max_timestamp: Option<u64>,
    ) -> Self {
        self.target_block_number = target_block_number;
        self.min_timestamp = min_timestamp;
        self.max_timestamp = max_timestamp;
        self
    }

    /// Returns the estimated compressed size of a transaction in bytes.
    /// This value is computed based on the following formula:
    /// `max(minTransactionSize, intercept + fastlzCoef*fastlzSize) / 1e6`
    /// Uses cached EIP-2718 encoded bytes to avoid recomputing the encoding for each estimation.
    pub fn estimated_compressed_size(&self) -> u64 {
        *self
            .estimated_tx_compressed_size
            .get_or_init(|| base_common_flz::tx_estimated_size_fjord_bytes(self.encoded_2718()))
    }

    /// Returns lazily computed EIP-2718 encoded bytes of the transaction.
    pub fn encoded_2718(&self) -> &Bytes {
        self.encoded_2718.get_or_init(|| self.inner.transaction().encoded_2718().into())
    }

    /// Returns the timestamp (millis since Unix epoch) when this transaction was received.
    const fn inner_received_at(&self) -> u128 {
        self.received_at
    }
}

impl<Cons: SignedTransaction, Pooled> DataAvailabilitySized
    for BasePooledTransaction<Cons, Pooled>
{
    fn estimated_da_size(&self) -> u64 {
        self.estimated_compressed_size()
    }
}

impl<Cons, Pooled> PoolTransaction for BasePooledTransaction<Cons, Pooled>
where
    Cons: SignedTransaction + From<Pooled>,
    Pooled: SignedTransaction + TryFrom<Cons, Error: core::error::Error>,
{
    type TryFromConsensusError = <Pooled as TryFrom<Cons>>::Error;
    type Consensus = Cons;
    type Pooled = Pooled;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.inner.transaction().clone()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.inner.transaction
    }

    fn into_consensus_with2718(self) -> WithEncoded<Recovered<Self::Consensus>> {
        let encoding = self.encoded_2718().clone();
        self.inner.transaction.into_encoded_with(encoding)
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        let encoded_len = tx.encode_2718_len();
        Self::new(tx.convert(), encoded_len)
    }

    fn hash(&self) -> &TxHash {
        self.inner.transaction.tx_hash()
    }

    fn sender(&self) -> Address {
        self.inner.transaction.signer()
    }

    fn sender_ref(&self) -> &Address {
        self.inner.transaction.signer_ref()
    }

    fn cost(&self) -> &U256 {
        &self.inner.cost
    }

    fn encoded_length(&self) -> usize {
        self.inner.encoded_length
    }
}

impl<Cons: Typed2718, Pooled> Typed2718 for BasePooledTransaction<Cons, Pooled> {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl<Cons: InMemorySize, Pooled> InMemorySize for BasePooledTransaction<Cons, Pooled> {
    fn size(&self) -> usize {
        self.inner.size() + core::mem::size_of::<u128>() + core::mem::size_of::<Option<u64>>() * 3
    }
}

impl<Cons, Pooled> alloy_consensus::Transaction for BasePooledTransaction<Cons, Pooled>
where
    Cons: alloy_consensus::Transaction,
    Pooled: Debug + Send + Sync + 'static,
{
    fn chain_id(&self) -> Option<u64> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.inner.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn is_create(&self) -> bool {
        self.inner.is_create()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

impl<Cons, Pooled> EthPoolTransaction for BasePooledTransaction<Cons, Pooled>
where
    Cons: SignedTransaction + From<Pooled>,
    Pooled: SignedTransaction + TryFrom<Cons>,
    <Pooled as TryFrom<Cons>>::Error: core::error::Error,
{
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        EthBlobTransactionSidecar::None
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        None
    }

    fn validate_blob(
        &self,
        _sidecar: &BlobTransactionSidecarVariant,
        _settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        Err(BlobTransactionValidationError::NotBlobTransaction(self.ty()))
    }
}

/// Helper trait to provide payload builder with access to encoded bytes of
/// transaction.
pub trait BasePooledTx: PoolTransaction + DataAvailabilitySized {
    /// Returns the EIP-2718 encoded bytes of the transaction.
    fn encoded_2718(&self) -> Cow<'_, Bytes>;
}

impl<Cons, Pooled> BasePooledTx for BasePooledTransaction<Cons, Pooled>
where
    Cons: SignedTransaction + From<Pooled>,
    Pooled: SignedTransaction + TryFrom<Cons>,
    <Pooled as TryFrom<Cons>>::Error: core::error::Error,
{
    fn encoded_2718(&self) -> Cow<'_, Bytes> {
        Cow::Borrowed(self.encoded_2718())
    }
}

/// Trait for transactions that expose their received-at timestamp.
pub trait TimestampedTransaction {
    /// Returns the time (millis since Unix epoch) when this transaction was received.
    fn received_at(&self) -> u128;
}

impl<Cons, Pooled> TimestampedTransaction for BasePooledTransaction<Cons, Pooled>
where
    Cons: SignedTransaction,
    Pooled: Send + Sync + 'static,
{
    fn received_at(&self) -> u128 {
        self.inner_received_at()
    }
}

/// Trait for transactions that may carry bundle metadata.
///
/// All timestamp values are in milliseconds since Unix epoch. Block-timestamp
/// arguments (which arrive in seconds) are converted internally.
pub trait BundleTransaction {
    /// Returns the target block number, if set.
    fn target_block_number(&self) -> Option<u64>;

    /// Returns the minimum timestamp in milliseconds.
    fn min_timestamp_millis(&self) -> Option<u64>;

    /// Returns the maximum timestamp in milliseconds.
    fn max_timestamp_millis(&self) -> Option<u64>;

    /// Returns `true` if this transaction's bundle constraints have expired
    /// relative to the given block number and block timestamp (in seconds).
    fn is_bundle_expired(&self, block_number: u64, block_timestamp_secs: u64) -> bool {
        let block_timestamp_millis = block_timestamp_secs.saturating_mul(1000);

        if let Some(max_ts) = self.max_timestamp_millis()
            && block_timestamp_millis > max_ts
        {
            return true;
        }

        if let Some(target) = self.target_block_number()
            && block_number > target
        {
            return true;
        }

        false
    }

    /// Returns `true` if this transaction's `min_timestamp` has not yet been
    /// reached. `block_timestamp_secs` is the block timestamp in seconds.
    fn is_bundle_not_yet_valid(&self, block_timestamp_secs: u64) -> bool {
        let block_timestamp_millis = block_timestamp_secs.saturating_mul(1000);

        if let Some(min_ts) = self.min_timestamp_millis()
            && block_timestamp_millis < min_ts
        {
            return true;
        }

        false
    }
}

impl<Cons, Pooled> BundleTransaction for BasePooledTransaction<Cons, Pooled>
where
    Cons: Send + Sync,
    Pooled: Send + Sync + 'static,
{
    fn target_block_number(&self) -> Option<u64> {
        self.target_block_number
    }

    fn min_timestamp_millis(&self) -> Option<u64> {
        self.min_timestamp
    }

    fn max_timestamp_millis(&self) -> Option<u64> {
        self.max_timestamp
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{TxKind, U256};
    use base_common_consensus::{BasePrimitives, BaseTransactionSigned, TxDeposit};
    use base_execution_chainspec::BASE_MAINNET;
    use base_execution_evm::BaseEvmConfig;
    use reth_provider::test_utils::MockEthProvider;
    use reth_transaction_pool::{
        TransactionOrigin, TransactionValidationOutcome, blobstore::InMemoryBlobStore,
        validate::EthTransactionValidatorBuilder,
    };

    use crate::{BasePooledTransaction, BaseTransactionValidator};
    #[tokio::test]
    async fn validate_base_transaction() {
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(BASE_MAINNET.clone())
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(BASE_MAINNET.clone());
        let validator = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        let validator = BaseTransactionValidator::new(validator);

        let origin = TransactionOrigin::External;
        let signer = Default::default();
        let deposit_tx = TxDeposit {
            source_hash: Default::default(),
            from: signer,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 0,
            is_system_transaction: false,
            input: Default::default(),
        };
        let signed_tx: BaseTransactionSigned = deposit_tx.into();
        let signed_recovered = Recovered::new_unchecked(signed_tx, signer);
        let len = signed_recovered.encode_2718_len();
        let pooled_tx: BasePooledTransaction = BasePooledTransaction::new(signed_recovered, len);
        let outcome = validator.validate_one(origin, pooled_tx).await;

        let err = match outcome {
            TransactionValidationOutcome::Invalid(_, err) => err,
            _ => panic!("Expected invalid transaction"),
        };
        assert_eq!(err.to_string(), "transaction type not supported");
    }
}
