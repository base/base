use core::fmt::Debug;
use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};

use alloy_consensus::{
    BlobTransactionValidationError, Transaction as _, Typed2718, transaction::Recovered,
};
use alloy_eips::{
    eip2718::{Encodable2718, WithEncoded},
    eip2930::AccessList,
    eip7594::BlobTransactionSidecarVariant,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use base_common_consensus::{BaseTransactionSigned, Eip8130Constants, Eip8130Signed};
use c_kzg::KzgSettings;
use reth_primitives_traits::{InMemorySize, SignedTransaction};
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, EthPooledTransaction, PoolTransaction,
};

use crate::estimated_da_size::DataAvailabilitySized;

/// A sequential nonce lane in the Base transaction pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseTransactionLane {
    /// The sender's protocol account-nonce lane.
    Protocol {
        /// Transaction sender.
        sender: Address,
    },
    /// A finite non-zero EIP-8130 nonce-key lane.
    Channel {
        /// Transaction sender.
        sender: Address,
        /// EIP-8130 nonce key.
        nonce_key: U256,
    },
}

/// Canonical transaction identity used for lane-aware routing and sequencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseTransactionIdentity {
    /// A nonce-bearing transaction identified within a sequential lane.
    Nonce {
        /// Sequential lane containing the transaction.
        lane: BaseTransactionLane,
        /// Nonce or nonce sequence within the lane.
        nonce: u64,
    },
    /// An independent nonce-free EIP-8130 transaction.
    Replay {
        /// Replay identifier committed to by the transaction.
        replay_id: B256,
    },
}

impl BaseTransactionIdentity {
    /// Derives the canonical identity from consensus transaction properties.
    pub fn new(sender: Address, nonce: u64, eip8130: Option<&Eip8130Signed>) -> Self {
        let Some(signed) = eip8130 else {
            return Self::Nonce { lane: BaseTransactionLane::Protocol { sender }, nonce };
        };
        let nonce_key = signed.tx().nonce_key;
        if nonce_key.is_zero() {
            Self::Nonce { lane: BaseTransactionLane::Protocol { sender }, nonce }
        } else if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            Self::Replay { replay_id: signed.tx().replay_id(sender) }
        } else {
            Self::Nonce { lane: BaseTransactionLane::Channel { sender, nonce_key }, nonce }
        }
    }

    /// Returns the sequential lane, or `None` for an independent replay identity.
    pub const fn lane(self) -> Option<BaseTransactionLane> {
        match self {
            Self::Nonce { lane, .. } => Some(lane),
            Self::Replay { .. } => None,
        }
    }

    /// Returns whether this identity is stored in the EIP-8130 sidecar.
    pub const fn is_sidecar(self) -> bool {
        matches!(
            self,
            Self::Nonce { lane: BaseTransactionLane::Channel { .. }, .. } | Self::Replay { .. }
        )
    }

    /// Returns whether this is an independent nonce-free replay identity.
    pub const fn is_replay(self) -> bool {
        matches!(self, Self::Replay { .. })
    }

    /// Returns whether this identity advances a protocol account nonce.
    pub const fn is_protocol(self) -> bool {
        matches!(self, Self::Nonce { lane: BaseTransactionLane::Protocol { .. }, .. })
    }
}

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

/// Pool transaction for Base.
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
    /// State predicates that must hold before this transaction is eligible for
    /// inclusion.
    validity_predicates: Vec<crate::ValidityPredicate>,
    /// The set of on-chain state surfaces whose change invalidates this
    /// transaction, computed once during validation and consumed by the pool's
    /// invalidation index. Empty until set; see [`crate::WatchSet`].
    watch_set: OnceLock<crate::WatchSet>,
    /// The admission limit classification (resolved sender/payer, lock/trusted
    /// status, payer balance and max cost), computed once during validation and
    /// consumed by the pool's admission guard. Unset until classified; see
    /// [`crate::LimitClass`].
    limit_class: OnceLock<crate::LimitClass>,
    /// The authorization read-set and build-time predicates captured during
    /// EIP-8130 validation. Unset for other transaction types; see
    /// [`crate::WatchManifest`].
    watch_manifest: OnceLock<crate::WatchManifest>,
}

impl<Cons: SignedTransaction, Pooled> BasePooledTransaction<Cons, Pooled> {
    /// Create new instance of [Self].
    pub fn new(transaction: Recovered<Cons>, encoded_length: usize) -> Self {
        Self::new_with_received_at(transaction, encoded_length, unix_time_millis())
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
            validity_predicates: Vec::new(),
            watch_set: OnceLock::new(),
            limit_class: OnceLock::new(),
            watch_manifest: OnceLock::new(),
        }
    }

    /// Sets the state predicates required for this transaction's inclusion.
    #[must_use]
    pub fn with_validity_predicates(
        mut self,
        validity_predicates: Vec<crate::ValidityPredicate>,
    ) -> Self {
        self.validity_predicates = validity_predicates;
        self
    }

    /// Returns the state predicates required for this transaction's inclusion.
    #[must_use]
    pub fn validity_predicates(&self) -> &[crate::ValidityPredicate] {
        &self.validity_predicates
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
}

impl<Cons: SignedTransaction, Pooled> DataAvailabilitySized
    for BasePooledTransaction<Cons, Pooled>
{
    fn estimated_da_size(&self) -> u64 {
        self.estimated_compressed_size()
    }
}

impl<Pooled> PoolTransaction for BasePooledTransaction<BaseTransactionSigned, Pooled>
where
    BaseTransactionSigned: From<Pooled>,
    Pooled: SignedTransaction + TryFrom<BaseTransactionSigned, Error: core::error::Error>,
{
    type TryFromConsensusError = <Pooled as TryFrom<BaseTransactionSigned>>::Error;
    type Consensus = BaseTransactionSigned;
    type Pooled = Pooled;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.inner.transaction().clone()
    }

    fn consensus_ref(&self) -> Recovered<&Self::Consensus> {
        self.inner.transaction().as_recovered_ref()
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
        alloy_consensus::transaction::TxHashRef::tx_hash(self.inner.transaction.inner())
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

    fn requires_nonce_check(&self) -> bool {
        BaseTransactionIdentity::new(self.sender(), self.nonce(), self.as_eip8130()).is_protocol()
    }
}

impl<Cons: Typed2718, Pooled> Typed2718 for BasePooledTransaction<Cons, Pooled> {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl<Cons: InMemorySize, Pooled> InMemorySize for BasePooledTransaction<Cons, Pooled> {
    fn size(&self) -> usize {
        let watch_keys_size =
            self.watch_set.get().map_or(0, |watch_set| core::mem::size_of_val(watch_set.keys()));
        let manifest_slots_size = self
            .watch_manifest
            .get()
            .map_or(0, |manifest| core::mem::size_of_val(manifest.config_slots()));
        let validity_predicates_size = core::mem::size_of_val(self.validity_predicates.as_slice());
        self.inner.size()
            + core::mem::size_of::<u128>()
            + core::mem::size_of::<Vec<crate::ValidityPredicate>>()
            + core::mem::size_of::<OnceLock<crate::WatchSet>>()
            + watch_keys_size
            + core::mem::size_of::<OnceLock<crate::LimitClass>>()
            + core::mem::size_of::<OnceLock<crate::WatchManifest>>()
            + manifest_slots_size
            + validity_predicates_size
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

impl<Pooled> EthPoolTransaction for BasePooledTransaction<BaseTransactionSigned, Pooled>
where
    BaseTransactionSigned: From<Pooled>,
    Pooled: SignedTransaction + TryFrom<BaseTransactionSigned>,
    <Pooled as TryFrom<BaseTransactionSigned>>::Error: core::error::Error,
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

    /// Returns state predicates required for this transaction's inclusion.
    ///
    /// Defaults to an empty slice for transaction types that do not carry
    /// validity predicates.
    fn validity_predicates(&self) -> &[crate::ValidityPredicate] {
        &[]
    }

    /// Returns the signed EIP-8130 payload when this transaction carries one.
    ///
    /// Required for the mempool validator's structural admission checks; the
    /// default returns `None` for implementers that never carry EIP-8130
    /// (account abstraction) transactions.
    fn as_eip8130(&self) -> Option<&Eip8130Signed> {
        None
    }

    /// Returns the canonical lane-aware identity for this transaction.
    fn identity(&self) -> BaseTransactionIdentity {
        BaseTransactionIdentity::new(self.sender(), self.nonce(), self.as_eip8130())
    }

    /// Returns the invalidation watch set computed during validation, if set.
    ///
    /// Defaults to `None` for implementers that do not track invalidation
    /// surfaces.
    fn watch_set(&self) -> Option<&crate::WatchSet> {
        None
    }

    /// Records the invalidation watch set computed during validation.
    ///
    /// Defaults to a no-op for implementers that do not track invalidation
    /// surfaces.
    fn set_watch_set(&self, _watch_set: crate::WatchSet) {}

    /// Returns the admission limit classification computed during validation, if
    /// set. Defaults to `None`.
    fn limit_class(&self) -> Option<&crate::LimitClass> {
        None
    }

    /// Records the admission limit classification computed during validation.
    /// Defaults to a no-op.
    fn set_limit_class(&self, _limit_class: crate::LimitClass) {}

    /// Returns build-time predicates captured during EIP-8130 authorization.
    ///
    /// Defaults to `None` for transaction types that do not carry a manifest.
    fn watch_manifest(&self) -> Option<&crate::WatchManifest> {
        None
    }

    /// Records build-time predicates captured during EIP-8130 authorization.
    ///
    /// Defaults to a no-op for transaction types that do not carry a manifest.
    fn set_watch_manifest(&self, _watch_manifest: crate::WatchManifest) {}
}

impl<Pooled> BasePooledTx for BasePooledTransaction<BaseTransactionSigned, Pooled>
where
    BaseTransactionSigned: From<Pooled>,
    Pooled: SignedTransaction + TryFrom<BaseTransactionSigned>,
    <Pooled as TryFrom<BaseTransactionSigned>>::Error: core::error::Error,
{
    fn encoded_2718(&self) -> Cow<'_, Bytes> {
        Cow::Borrowed(self.encoded_2718())
    }

    fn validity_predicates(&self) -> &[crate::ValidityPredicate] {
        &self.validity_predicates
    }

    fn as_eip8130(&self) -> Option<&Eip8130Signed> {
        self.inner.transaction().inner().as_eip8130()
    }

    fn watch_set(&self) -> Option<&crate::WatchSet> {
        self.watch_set.get()
    }

    fn set_watch_set(&self, watch_set: crate::WatchSet) {
        let _ = self.watch_set.set(watch_set);
    }

    fn limit_class(&self) -> Option<&crate::LimitClass> {
        self.limit_class.get()
    }

    fn watch_manifest(&self) -> Option<&crate::WatchManifest> {
        self.watch_manifest.get()
    }

    fn set_watch_manifest(&self, watch_manifest: crate::WatchManifest) {
        let _ = self.watch_manifest.set(watch_manifest);
    }

    fn set_limit_class(&self, limit_class: crate::LimitClass) {
        let _ = self.limit_class.set(limit_class);
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
        self.received_at
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives, BaseTransactionSigned,
        Eip8130Constants, Eip8130Signed, TxDeposit, TxEip8130,
    };
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use base_test_utils::Account;
    use reth_primitives_traits::InMemorySize;
    use reth_provider::test_utils::MockEthProvider;
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin, TransactionValidationOutcome,
        blobstore::InMemoryBlobStore, test_utils::TransactionBuilder,
        validate::EthTransactionValidatorBuilder,
    };

    use crate::{
        BasePooledTransaction, BasePooledTx, BaseTransactionIdentity, BaseTransactionLane,
        BaseTransactionValidator, ConfigSlot, InvalidationKey, ValidityOperator, ValidityPredicate,
        WatchManifest, WatchSet,
    };

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn eip8130_pooled(nonce_key: U256) -> BasePooledTransaction {
        let signer = signer();
        eip8130_pooled_for(&signer, nonce_key, 0)
    }

    fn eip8130_pooled_for(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before: if nonce_key == Eip8130Constants::NONCE_KEY_MAX { 5 } else { 0 },
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn ordinary_pooled(nonce: u64) -> BasePooledTransaction {
        let account = Account::Alice;
        let signed = TransactionBuilder::default()
            .signer(account.signer_b256())
            .chain_id(ChainConfig::mainnet().chain_id)
            .nonce(nonce)
            .to(Account::Bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();
        let transaction = BaseTransactionSigned::Eip1559(
            signed.as_eip1559().expect("EIP-1559 transaction").clone(),
        );
        let recovered = Recovered::new_unchecked(transaction, account.address());
        let encoded_length = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, encoded_length)
    }

    #[tokio::test]
    async fn validate_base_transaction() {
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(chain_spec);
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

    #[test]
    fn nonce_free_eip8130_skips_protocol_nonce_check() {
        assert!(eip8130_pooled(U256::ZERO).requires_nonce_check());
        assert!(!eip8130_pooled(U256::from(1)).requires_nonce_check());
        assert!(!eip8130_pooled(Eip8130Constants::NONCE_KEY_MAX).requires_nonce_check());
    }

    #[test]
    fn identity_distinguishes_protocol_channels_and_replays() {
        let signer = PrivateKeySigner::from_bytes(&Account::Alice.signer_b256()).unwrap();
        let ordinary = ordinary_pooled(7).identity();
        let key_zero = eip8130_pooled_for(&signer, U256::ZERO, 7).identity();
        let channel_one = eip8130_pooled_for(&signer, U256::from(1), 7).identity();
        let channel_two = eip8130_pooled_for(&signer, U256::from(2), 7).identity();
        let replay = eip8130_pooled_for(&signer, Eip8130Constants::NONCE_KEY_MAX, 7).identity();

        assert!(matches!(
            ordinary,
            BaseTransactionIdentity::Nonce { lane: BaseTransactionLane::Protocol { .. }, nonce: 7 }
        ));
        assert!(matches!(
            key_zero,
            BaseTransactionIdentity::Nonce { lane: BaseTransactionLane::Protocol { .. }, nonce: 7 }
        ));
        assert_eq!(ordinary, key_zero);
        assert!(ordinary.is_protocol());
        assert!(key_zero.is_protocol());
        assert_ne!(channel_one, channel_two);
        assert!(channel_one.is_sidecar());
        assert!(!channel_one.is_protocol());
        assert!(replay.is_sidecar());
        assert!(replay.is_replay());
        assert!(!replay.is_protocol());
        assert_eq!(replay.lane(), None);
    }

    #[test]
    fn in_memory_size_includes_watch_keys() {
        let transaction = eip8130_pooled(U256::ZERO);
        let size_without_keys = transaction.size();
        let watch_set = WatchSet::new()
            .watch(InvalidationKey::Balance(Address::ZERO))
            .watch(InvalidationKey::ProtocolNonce(Address::ZERO));
        let keys_size = core::mem::size_of_val(watch_set.keys());

        transaction.set_watch_set(watch_set);

        assert_eq!(transaction.size(), size_without_keys + keys_size);
    }

    #[test]
    fn in_memory_size_includes_manifest_slots() {
        let transaction = eip8130_pooled(U256::ZERO);
        let size_without_slots = transaction.size();
        let manifest = WatchManifest::new(
            vec![
                ConfigSlot { address: Address::ZERO, slot: U256::ZERO, expected: U256::ZERO },
                ConfigSlot {
                    address: Address::repeat_byte(1),
                    slot: U256::from(1),
                    expected: U256::from(2),
                },
            ],
            Address::ZERO,
            U256::ZERO,
            u64::MAX,
        );
        let slots_size = core::mem::size_of_val(manifest.config_slots());

        transaction.set_watch_manifest(manifest);

        assert_eq!(transaction.size(), size_without_slots + slots_size);
    }

    #[test]
    fn retains_validity_predicates() {
        let predicate = ValidityPredicate::Balance {
            address: Address::repeat_byte(1),
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(1),
        };
        let transaction =
            eip8130_pooled(U256::ZERO).with_validity_predicates(vec![predicate.clone()]);

        assert_eq!(transaction.validity_predicates(), core::slice::from_ref(&predicate));
        assert_eq!(
            BasePooledTx::validity_predicates(&transaction),
            core::slice::from_ref(&predicate)
        );
    }
}
