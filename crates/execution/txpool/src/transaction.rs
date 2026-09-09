use core::fmt::Debug;
use std::sync::OnceLock;

use alloy_eips::{
    eip2718::{Decodable2718, Encodable2718, WithEncoded},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use base_common_types_chain::{
    BasePooledTransaction as BasePooledEnvelope, BaseTransactionSigned, Eip8130Constants,
    Eip8130Signed, Transaction, Typed2718, transaction::Recovered,
};
use reth_primitives_traits::{InMemorySize, SignedTransaction};

use crate::{
    InvalidPoolTransactionError, RawPoolTransactionError, estimated_da_size::DataAvailabilitySized,
};

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
#[derive(Debug, Clone)]
pub struct BasePooledTransaction {
    /// Recovered Base transaction.
    pub transaction: Recovered<BaseTransactionSigned>,
    /// Maximum execution cost, including transferred value.
    pub cost: U256,
    /// Encoded transaction length computed on admission.
    pub encoded_length: usize,
    /// The estimated size of this transaction, lazily computed.
    estimated_tx_compressed_size: OnceLock<u64>,
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

impl BasePooledTransaction {
    /// Create new instance of [Self].
    pub fn new(transaction: Recovered<BaseTransactionSigned>, encoded_length: usize) -> Self {
        Self::new_with_received_at(transaction, encoded_length, unix_time_millis())
    }

    /// Create new instance with an explicit `received_at` timestamp (millis since Unix epoch).
    ///
    /// Primarily for testing.
    pub fn new_with_received_at(
        transaction: Recovered<BaseTransactionSigned>,
        encoded_length: usize,
        received_at: u128,
    ) -> Self {
        let cost = U256::from(transaction.max_fee_per_gas())
            .saturating_mul(U256::from(transaction.gas_limit()))
            .saturating_add(transaction.value());
        Self {
            transaction,
            cost,
            encoded_length,
            estimated_tx_compressed_size: Default::default(),
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
        self.encoded_2718.get_or_init(|| self.transaction.encoded_2718().into())
    }
}

impl DataAvailabilitySized for BasePooledTransaction {
    fn estimated_da_size(&self) -> u64 {
        self.estimated_compressed_size()
    }
}

impl BasePooledTransaction {
    /// Clone the transaction into a consensus variant.
    ///
    pub fn clone_into_consensus(&self) -> Recovered<BaseTransactionSigned> {
        self.transaction.clone()
    }

    /// Returns a reference to the consensus transaction with the recovered sender.
    pub fn consensus_ref(&self) -> Recovered<&BaseTransactionSigned> {
        self.transaction.as_recovered_ref()
    }

    /// Define a method to convert from the `Self` type to `Consensus`
    pub fn into_consensus(self) -> Recovered<BaseTransactionSigned> {
        self.transaction
    }

    /// Converts the transaction into consensus format while preserving the EIP-2718 encoded bytes.
    /// This is used to optimize transaction execution by reusing cached encoded bytes instead of
    /// re-encoding the transaction. The cached bytes are particularly useful in payload building
    /// where the same transaction may be executed multiple times.
    pub fn into_consensus_with2718(self) -> WithEncoded<Recovered<BaseTransactionSigned>> {
        let encoding = self.encoded_2718().clone();
        self.transaction.into_encoded_with(encoding)
    }

    /// Define a method to convert from the `Pooled` type to `Self`
    pub fn from_pooled(tx: Recovered<BasePooledEnvelope>) -> Self {
        let encoded_len = tx.encode_2718_len();
        Self::new(tx.convert(), encoded_len)
    }

    /// Hash of the transaction.
    pub fn hash(&self) -> &TxHash {
        base_common_types_chain::transaction::TxHashRef::tx_hash(self.transaction.inner())
    }

    /// The Sender of the transaction.
    pub fn sender(&self) -> Address {
        self.transaction.signer()
    }

    /// Reference to the Sender of the transaction.
    pub fn sender_ref(&self) -> &Address {
        self.transaction.signer_ref()
    }

    /// Returns the cost that this transaction is allowed to consume:
    ///
    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    pub fn cost(&self) -> &U256 {
        &self.cost
    }

    /// Returns the length of the rlp encoded transaction object
    ///
    /// Note: Implementations should cache this value.
    pub fn encoded_length(&self) -> usize {
        self.encoded_length
    }

    /// Allows to communicate to the pool that the transaction doesn't require a nonce check.
    pub fn requires_nonce_check(&self) -> bool {
        self.as_eip8130().is_none_or(|signed| signed.tx().nonce_key.is_zero())
    }

    /// Define a method to convert from the `Consensus` type to `Self`
    ///
    /// This conversion may fail for transactions that are valid for inclusion in blocks
    /// but cannot exist in the transaction pool. Examples include:
    ///
    /// - **OP Deposit transactions**: These are special system transactions that are directly
    ///   included in blocks by the sequencer/validator and never enter the mempool
    pub fn try_from_consensus(
        tx: Recovered<BaseTransactionSigned>,
    ) -> Result<Self, <BasePooledEnvelope as TryFrom<BaseTransactionSigned>>::Error> {
        let (tx, signer) = tx.into_parts();
        Ok(Self::from_pooled(Recovered::new_unchecked(tx.try_into()?, signer)))
    }

    /// Recovers and converts a pooled transaction into this pool transaction type.
    ///
    pub fn try_recover(pooled: BasePooledEnvelope) -> Result<Self, BasePooledEnvelope> {
        pooled.try_into_recovered().map(Self::from_pooled)
    }

    /// Recovers and converts a pooled transaction using the provided sender recovery cache.
    pub fn try_recover_with_cache(
        pooled: BasePooledEnvelope,
        cache: &base_execution_evm::SenderRecoveryCache,
    ) -> Result<Self, BasePooledEnvelope> {
        match cache.recover(&pooled) {
            Ok(signer) => Ok(Self::from_pooled(Recovered::new_unchecked(pooled, signer))),
            Err(_) => Err(pooled),
        }
    }

    /// Decodes and recovers a raw transaction into this pool transaction type.
    ///
    pub fn recover_raw_transaction(data: &[u8]) -> Result<Self, RawPoolTransactionError> {
        if data.is_empty() {
            return Err(RawPoolTransactionError::EmptyRawTransactionData);
        }

        let transaction = BasePooledEnvelope::decode_2718_exact(data)
            .map_err(|_| RawPoolTransactionError::FailedToDecodeSignedTransaction)?;

        Self::try_recover(transaction)
            .map_err(|_| RawPoolTransactionError::InvalidTransactionSignature)
    }

    /// Tries to convert the `Consensus` type into the `Pooled` type.
    pub fn try_into_pooled(
        self,
    ) -> Result<
        Recovered<BasePooledEnvelope>,
        <BasePooledEnvelope as TryFrom<BaseTransactionSigned>>::Error,
    > {
        let consensus = self.into_consensus();
        let (tx, signer) = consensus.into_parts();
        Ok(Recovered::new_unchecked(tx.try_into()?, signer))
    }

    /// Clones the consensus transactions and tries to convert the `Consensus` type into the
    /// `Pooled` type.
    pub fn clone_into_pooled(
        &self,
    ) -> Result<
        Recovered<BasePooledEnvelope>,
        <BasePooledEnvelope as TryFrom<BaseTransactionSigned>>::Error,
    > {
        let consensus = self.clone_into_consensus();
        let (tx, signer) = consensus.into_parts();
        Ok(Recovered::new_unchecked(tx.try_into()?, signer))
    }

    /// Converts the `Pooled` type into the `Consensus` type.
    pub fn pooled_into_consensus(tx: BasePooledEnvelope) -> BaseTransactionSigned {
        tx.into()
    }

    /// Ensures that the transaction's code size does not exceed the provided `max_init_code_size`.
    ///
    /// This is specifically relevant for contract creation transactions ([`TxKind::Create`]),
    /// where the input data contains the initialization code. If the input code size exceeds
    /// the configured limit, an [`InvalidPoolTransactionError::ExceedsMaxInitCodeSize`] error is
    /// returned.
    pub fn ensure_max_init_code_size(
        &self,
        max_init_code_size: usize,
    ) -> Result<(), InvalidPoolTransactionError> {
        let input_len = self.input().len();
        if self.is_create() && input_len > max_init_code_size {
            Err(InvalidPoolTransactionError::ExceedsMaxInitCodeSize(input_len, max_init_code_size))
        } else {
            Ok(())
        }
    }
}

impl Typed2718 for BasePooledTransaction {
    fn ty(&self) -> u8 {
        self.transaction.ty()
    }
}

impl InMemorySize for BasePooledTransaction {
    fn size(&self) -> usize {
        let watch_keys_size =
            self.watch_set.get().map_or(0, |watch_set| core::mem::size_of_val(watch_set.keys()));
        let manifest_slots_size = self
            .watch_manifest
            .get()
            .map_or(0, |manifest| core::mem::size_of_val(manifest.config_slots()));
        let validity_predicates_size = core::mem::size_of_val(self.validity_predicates.as_slice());
        self.transaction.size()
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

impl base_common_types_chain::Transaction for BasePooledTransaction {
    fn chain_id(&self) -> Option<u64> {
        self.transaction.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.transaction.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.transaction.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.transaction.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.transaction.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.transaction.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.transaction.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.transaction.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.transaction.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.transaction.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.transaction.kind()
    }

    fn is_create(&self) -> bool {
        self.transaction.is_create()
    }

    fn value(&self) -> U256 {
        self.transaction.value()
    }

    fn input(&self) -> &Bytes {
        self.transaction.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.transaction.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.transaction.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.transaction.authorization_list()
    }
}

impl BasePooledTransaction {
    /// Whether this transaction uses a nonstandard nonce channel or replay identifier.
    pub fn is_eip8130_sidecar_transaction(&self) -> bool {
        self.eip8130_nonce_channel_key().is_some() || self.eip8130_replay_id().is_some()
    }

    /// Returns the signed EIP-8130 payload, when present.
    pub fn as_eip8130(&self) -> Option<&Eip8130Signed> {
        self.transaction.inner().as_eip8130()
    }

    /// Returns the finite nonzero nonce channel managed by the 2D nonce pool.
    pub fn eip8130_nonce_channel_key(&self) -> Option<U256> {
        let signed = self.as_eip8130()?;
        let nonce_key = signed.tx().nonce_key;
        (!nonce_key.is_zero() && nonce_key != Eip8130Constants::NONCE_KEY_MAX).then_some(nonce_key)
    }

    /// Returns the replay identifier for a nonce-free EIP-8130 transaction.
    pub fn eip8130_replay_id(&self) -> Option<B256> {
        let signed = self.as_eip8130()?;
        // `replay_id` keys mempool dedup/replacement only for nonce-free
        // (`nonce_key == NONCE_KEY_MAX`) transactions, which have no nonce slot.
        // Standard and 2D transactions dedupe/replace on
        // `(sender, nonce_key, nonce_sequence)` under the standard nonce rules,
        // so they must not be tracked by `replay_id` (which excludes fees and
        // would otherwise block legitimate replace-by-fee at the same sequence).
        if signed.tx().nonce_key != Eip8130Constants::NONCE_KEY_MAX {
            return None;
        }
        Some(signed.tx().replay_id(self.sender()))
    }

    /// Returns the invalidation watch set recorded during validation.
    pub fn watch_set(&self) -> Option<&crate::WatchSet> {
        self.watch_set.get()
    }

    /// Records the invalidation watch set once during validation.
    pub fn set_watch_set(&self, watch_set: crate::WatchSet) {
        let _ = self.watch_set.set(watch_set);
    }

    /// Returns the admission classification recorded during validation.
    pub fn limit_class(&self) -> Option<&crate::LimitClass> {
        self.limit_class.get()
    }

    /// Returns the build-time predicates captured during authorization.
    pub fn watch_manifest(&self) -> Option<&crate::WatchManifest> {
        self.watch_manifest.get()
    }

    /// Records the build-time predicates once during authorization.
    pub fn set_watch_manifest(&self, watch_manifest: crate::WatchManifest) {
        let _ = self.watch_manifest.set(watch_manifest);
    }

    /// Records the admission classification once during validation.
    pub fn set_limit_class(&self, limit_class: crate::LimitClass) {
        let _ = self.limit_class.set(limit_class);
    }
}

impl BasePooledTransaction {
    /// Returns the receipt timestamp in milliseconds since the Unix epoch.
    pub fn received_at(&self) -> u128 {
        self.received_at
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use alloy_signer::SignerSync;
    use base_common_chain_config::ChainConfig;
    use base_common_network::PrivateKeySigner;
    use base_common_types_chain::{
        BasePooledTransaction as ConsensusPooledTransaction, BaseTransactionSigned, BaseTxEnvelope,
        Eip8130Constants, Eip8130Signed, EthereumTxEnvelope, SignableTransaction, TxDeposit,
        TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxEip8130, TxLegacy, transaction::Recovered,
    };
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use base_execution_txpool::{
        EthTransactionValidatorBuilder, TransactionOrigin, TransactionValidationOutcome,
    };
    use reth_primitives_traits::InMemorySize;
    use reth_provider::test_utils::MockEthProvider;

    use crate::{
        BasePooledTransaction, BaseTransactionValidator, ConfigSlot, InvalidationKey,
        ValidityOperator, ValidityPredicate, WatchManifest, WatchSet,
    };

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn eip8130_pooled(nonce_key: U256) -> BasePooledTransaction {
        let signer = signer();
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence: 0,
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

    #[tokio::test]
    async fn validate_base_transaction() {
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        let client = MockEthProvider::new()
            .with_chain_spec(chain_spec.as_ref().clone())
            .with_genesis_block();
        let evm_config = BaseEvmConfig::new(chain_spec);
        let validator = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build();
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
        assert_eq!(transaction.validity_predicates(), core::slice::from_ref(&predicate));
    }

    #[test]
    fn test_base_pooled_transaction_new_legacy() {
        // Create a legacy transaction with specific parameters
        let tx = BaseTxEnvelope::Legacy(
            TxLegacy {
                gas_price: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = BasePooledTransaction::new(transaction.clone(), 200);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
    }

    #[test]
    fn test_base_pooled_transaction_new_eip2930() {
        // Create an EIP-2930 transaction with specific parameters
        let tx = BaseTxEnvelope::Eip2930(
            TxEip2930 {
                gas_price: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = BasePooledTransaction::new(transaction.clone(), 200);
        let expected_cost = U256::from(100) + (U256::from(10 * 1000));

        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.cost, expected_cost);
    }

    #[test]
    fn test_base_pooled_transaction_new_eip1559() {
        // Create an EIP-1559 transaction with specific parameters
        let tx = BaseTxEnvelope::Eip1559(
            TxEip1559 {
                max_fee_per_gas: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = BasePooledTransaction::new(transaction.clone(), 200);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
    }

    #[test]
    fn rejects_raw_blob_transaction() {
        let tx = EthereumTxEnvelope::Eip4844(
            TxEip4844 { blob_versioned_hashes: vec![B256::ZERO], ..Default::default() }
                .into_signed(Signature::test_signature()),
        );
        assert!(matches!(
            BasePooledTransaction::recover_raw_transaction(&tx.encoded_2718()),
            Err(crate::RawPoolTransactionError::FailedToDecodeSignedTransaction)
        ));
    }

    #[test]
    fn test_base_pooled_transaction_new_eip7702() {
        // Init an EIP-7702 transaction with specific parameters
        let tx = BaseTxEnvelope::Eip7702(
            TxEip7702 {
                max_fee_per_gas: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = BasePooledTransaction::new(transaction.clone(), 200);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
    }
}
