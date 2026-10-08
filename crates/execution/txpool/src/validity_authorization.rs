//! Sender authorization for off-chain validity predicates.

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolStruct, eip712_domain};
use reth_transaction_pool::PoolTransaction;

use crate::{
    BasePooledTransaction, Eip712ValidityAuthorization, Eip712ValidityPredicate,
    TransactionValidity, ValidityPredicate, ValiditySignatureMode,
};

/// Failure to authorize a validity sidecar with the transaction sender's key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidityAuthorizationError {
    /// Non-empty predicates require a sender signature.
    #[error("validity predicates require a sender signature")]
    MissingSignature,
    /// A signature without any predicates is not a valid sidecar.
    #[error("validity signature requires non-empty predicates")]
    UnexpectedSignature,
    /// Deposits and unprotected legacy transactions have no signing domain.
    #[error("signed validity predicates require a chain-protected transaction")]
    MissingChainId,
    /// The transaction's sender could not be recovered from its envelope.
    #[error("failed to recover validity transaction sender")]
    InvalidTransactionSender,
    /// A builder's pre-recovered sender differs from the actual envelope sender.
    #[error("validity transaction sender does not match its envelope")]
    SenderMismatch,
    /// A validated sidecar was attached to a different transaction.
    #[error("validity authorization belongs to a different transaction")]
    TransactionMismatch,
    /// The signature is malformed, non-canonical, or belongs to another key.
    #[error("invalid validity signature: must be signed by the transaction sender")]
    InvalidSignature,
}

impl ValidityAuthorizationError {
    /// Returns a stable, low-cardinality metric and RPC rejection reason.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::MissingSignature => "missing",
            Self::UnexpectedSignature => "unexpected",
            Self::MissingChainId => "missing_chain_id",
            Self::InvalidTransactionSender => "invalid_transaction_sender",
            Self::SenderMismatch => "sender_mismatch",
            Self::InvalidSignature => "invalid",
            Self::TransactionMismatch => "transaction_mismatch",
        }
    }
}

/// Policy-validated validity sidecar bound to the exact signed transaction hash.
///
/// Only [`ValidityAuthorization`] constructs this type. It deliberately has no
/// deserializer or public constructor. Unsigned sidecars may be accepted by the
/// configured rollout mode; validation does not imply that a signature was verified.
#[derive(Debug)]
pub struct ValidatedValidity {
    transaction_hash: B256,
    sidecar: TransactionValidity,
}

impl ValidatedValidity {
    /// Returns the original predicate batch for admission journaling.
    pub fn predicates(&self) -> &[ValidityPredicate] {
        &self.sidecar.validity
    }

    /// Releases the validated sidecar only for its original transaction.
    pub fn into_sidecar(
        self,
        transaction_hash: B256,
    ) -> Result<TransactionValidity, ValidityAuthorizationError> {
        if transaction_hash != self.transaction_hash {
            return Err(ValidityAuthorizationError::TransactionMismatch);
        }
        Ok(self.sidecar)
    }
}

/// EIP-712 signing and verification for transaction validity sidecars.
#[derive(Debug)]
pub struct ValidityAuthorization;

impl ValidityAuthorization {
    /// Validates a sidecar at the builder wire boundary, checking the actual
    /// envelope sender rather than trusting the wire's pre-recovered address.
    pub fn validate(
        tx: &BasePooledTransaction,
        sidecar: TransactionValidity,
        mode: ValiditySignatureMode,
    ) -> Result<ValidatedValidity, ValidityAuthorizationError> {
        mode.check(&sidecar.validity, sidecar.validity_signature.as_ref())?;
        if mode != ValiditySignatureMode::Off && sidecar.validity_signature.is_some() {
            let sender = tx
                .consensus_ref()
                .inner()
                .recover_signer()
                .map_err(|_| ValidityAuthorizationError::InvalidTransactionSender)?;
            if sender != tx.sender() {
                return Err(ValidityAuthorizationError::SenderMismatch);
            }
        }
        Self::validate_recovered(tx, sidecar, mode)
    }

    /// Validates a sidecar at raw ingress after `recover_raw_transaction` has
    /// recovered the sender. Never use this for a sender supplied on the wire.
    pub fn validate_recovered(
        tx: &BasePooledTransaction,
        sidecar: TransactionValidity,
        mode: ValiditySignatureMode,
    ) -> Result<ValidatedValidity, ValidityAuthorizationError> {
        mode.check(&sidecar.validity, sidecar.validity_signature.as_ref())?;
        if mode != ValiditySignatureMode::Off
            && let Some(signature) = &sidecar.validity_signature
        {
            let chain_id = tx.chain_id().ok_or(ValidityAuthorizationError::MissingChainId)?;
            if signature.normalize_s().is_some() {
                return Err(ValidityAuthorizationError::InvalidSignature);
            }
            let digest = Self::signing_hash(chain_id, *tx.hash(), &sidecar.validity);
            let signer = signature
                .recover_address_from_prehash(&digest)
                .map_err(|_| ValidityAuthorizationError::InvalidSignature)?;
            if signer != tx.sender() {
                return Err(ValidityAuthorizationError::InvalidSignature);
            }
        }
        Ok(ValidatedValidity { transaction_hash: *tx.hash(), sidecar })
    }

    /// Returns the EIP-712 digest a sender must sign to authorize these predicates.
    ///
    /// The domain is `Base Transaction Validity`, version `1`, with the transaction's
    /// chain ID. The message binds the complete signed transaction hash and all
    /// predicate fields. Sorting by EIP-712 struct hash makes conjunction order
    /// irrelevant, while retaining duplicates and normalizing omitted storage masks.
    pub fn signing_hash(
        chain_id: u64,
        transaction_hash: B256,
        predicates: &[ValidityPredicate],
    ) -> B256 {
        let message = Eip712ValidityAuthorization {
            transactionHash: transaction_hash,
            validity: Self::canonical_predicates(predicates),
        };
        message.eip712_signing_hash(&eip712_domain! {
            name: "Base Transaction Validity",
            version: "1",
            chain_id: chain_id,
        })
    }

    /// Returns wallet-facing predicates sorted by their EIP-712 struct hashes.
    /// Evaluation ordering remains independent; duplicate predicates are retained.
    pub fn canonical_predicates(predicates: &[ValidityPredicate]) -> Vec<Eip712ValidityPredicate> {
        let mut data: Vec<_> = predicates.iter().map(Self::predicate_data).collect();
        data.sort_by_cached_key(SolStruct::eip712_hash_struct);
        data
    }

    /// Converts a predicate to its canonical EIP-712 representation.
    pub const fn predicate_data(predicate: &ValidityPredicate) -> Eip712ValidityPredicate {
        let (account, slot, mask, op, value) = match predicate {
            ValidityPredicate::Balance { address, op, value }
            | ValidityPredicate::Nonce { address, op, value } => {
                (*address, U256::ZERO, U256::ZERO, *op, *value)
            }
            ValidityPredicate::Storage { address, slot, mask, op, value } => {
                (*address, *slot, *mask, *op, *value)
            }
            ValidityPredicate::BlockNumber { op, value }
            | ValidityPredicate::FlashblockIndex { op, value } => {
                (Address::ZERO, U256::ZERO, U256::ZERO, *op, *value)
            }
        };
        Eip712ValidityPredicate {
            kind: predicate.kind() as u8,
            operator: op as u8,
            account,
            slot,
            mask,
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Signature, TxKind};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
    use reth_transaction_pool::{
        TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    };

    use super::*;
    use crate::{
        TransactionValidity, ValidatedTransaction, ValidatedTransactionExtensions,
        ValidityOperator, ValidityPredicateKind,
    };

    fn verify(
        tx: &BasePooledTransaction,
        predicates: &[ValidityPredicate],
        signature: Option<&Signature>,
    ) -> Result<(), ValidityAuthorizationError> {
        ValidityAuthorization::validate(
            tx,
            TransactionValidity {
                validity: predicates.to_vec(),
                validity_signature: signature.copied(),
            },
            ValiditySignatureMode::Required,
        )
        .map(|_| ())
    }

    fn transaction(signer: &PrivateKeySigner, chain_id: u64, nonce: u64) -> BasePooledTransaction {
        let tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0x11)),
            ..Default::default()
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let signed =
            BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Eip1559(tx), signature);
        BasePooledTransaction::recover_raw_transaction(&signed.encoded_2718()).unwrap()
    }

    fn predicates() -> Vec<ValidityPredicate> {
        vec![
            ValidityPredicate::Balance {
                address: Address::repeat_byte(0x22),
                op: ValidityOperator::GreaterThan,
                value: U256::from(10),
            },
            ValidityPredicate::Storage {
                address: Address::repeat_byte(0x33),
                slot: U256::from(7),
                mask: U256::from(255),
                op: ValidityOperator::Equal,
                value: U256::from(1),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(3),
            },
        ]
    }

    fn sign(
        signer: &PrivateKeySigner,
        tx: &BasePooledTransaction,
        predicates: &[ValidityPredicate],
    ) -> Signature {
        signer
            .sign_hash_sync(&ValidityAuthorization::signing_hash(
                tx.chain_id().unwrap(),
                *tx.hash(),
                predicates,
            ))
            .unwrap()
    }

    #[test]
    fn eip712_digest_matches_independently_encoded_vector() {
        // Independently encoded with Keccak-256 and the documented EIP-712 types:
        // chain 8453, hash 0x44...44, balance/storage/block/flashblock predicates in canonical order.
        let expected: B256 =
            "0xccc8fc76b39e370a65b7e6c377ad2f7a00d309b80f5e100cf119b03f41de294d".parse().unwrap();
        assert_eq!(
            ValidityAuthorization::signing_hash(8453, B256::repeat_byte(0x44), &predicates()),
            expected,
        );
    }

    #[test]
    fn nonce_digest_matches_independently_encoded_vector() {
        let predicate = ValidityPredicate::Nonce {
            address: Address::repeat_byte(0x44),
            op: ValidityOperator::Equal,
            value: U256::from(7),
        };
        // Independent Keccak-256 encoding: nonce kind 4, operator 2, zero slot/mask.
        let expected: B256 =
            "0x021385c3ddaed95323b4f834b255046fb76b8a315ea935efaec986ae00d2ffec".parse().unwrap();
        assert_eq!(
            ValidityAuthorization::signing_hash(8453, B256::repeat_byte(0x44), &[predicate]),
            expected,
        );
    }

    #[test]
    fn omitted_storage_mask_signs_the_same_as_explicit_full_mask() {
        let omitted: ValidityPredicate = serde_json::from_str(
            r#"{"type":"storage","params":{"address":"0x3333333333333333333333333333333333333333","slot":"0x7","op":"=","value":"0x1"}}"#,
        ).unwrap();
        let explicit = ValidityPredicate::Storage {
            address: Address::repeat_byte(0x33),
            slot: U256::from(7),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(1),
        };
        assert_eq!(
            ValidityAuthorization::signing_hash(8453, B256::repeat_byte(0x44), &[omitted]),
            ValidityAuthorization::signing_hash(8453, B256::repeat_byte(0x44), &[explicit]),
        );
    }

    #[test]
    fn unprotected_legacy_transactions_cannot_authorize_predicates() {
        let signer = PrivateKeySigner::random();
        let legacy = TxLegacy { chain_id: None, gas_limit: 21_000, ..Default::default() };
        let signature = signer.sign_hash_sync(&legacy.signature_hash()).unwrap();
        let signed =
            BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Legacy(legacy), signature);
        let tx = BasePooledTransaction::recover_raw_transaction(&signed.encoded_2718()).unwrap();
        assert_eq!(
            verify(&tx, &predicates(), Some(&signature)),
            Err(ValidityAuthorizationError::MissingChainId),
        );
        assert!(
            ValidityAuthorization::validate(
                &tx,
                TransactionValidity { validity: predicates(), validity_signature: Some(signature) },
                ValiditySignatureMode::Off,
            )
            .is_ok()
        );
    }

    #[test]
    fn user_authorization_survives_sorting_and_builder_wire_forwarding() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let mut validity = predicates();
        validity.push(ValidityPredicate::Nonce {
            address: Address::repeat_byte(0x44),
            op: ValidityOperator::Equal,
            value: U256::from(7),
        });
        let signature = sign(&signer, &tx, &validity);
        assert_eq!(verify(&tx, &validity, Some(&signature)), Ok(()));
        let validity = ValidityAuthorization::validate_recovered(
            &tx,
            TransactionValidity { validity, validity_signature: Some(signature) },
            ValiditySignatureMode::Required,
        )
        .unwrap();
        let tx = tx.with_validity(validity).unwrap();
        let pooled = ValidPoolTransaction {
            transaction: tx,
            transaction_id: TransactionId::new(0.into(), 0),
            propagate: false,
            timestamp: std::time::Instant::now(),
            origin: TransactionOrigin::Private,
            authority_ids: None,
        };
        let wire = ValidatedTransaction {
            sender: pooled.transaction.sender(),
            raw: pooled.transaction.encoded_2718().clone(),
            metering: None,
            extensions: TransactionValidity::extract(&pooled),
        };
        let decoded: ValidatedTransaction<TransactionValidity> =
            serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();
        let inbound = BasePooledTransaction::recover_raw_transaction(&decoded.raw).unwrap();
        let inbound = decoded.extensions.apply(inbound, ValiditySignatureMode::Required).unwrap();
        assert_eq!(inbound.validity_signature(), Some(signature));
        assert_eq!(
            verify(&inbound, inbound.validity_predicates(), inbound.validity_signature().as_ref(),),
            Ok(())
        );
        // Replacing predicates through the unsigned API must not retain stale authorization.
        let unsigned = inbound.with_validity_predicates(predicates());
        assert_eq!(unsigned.validity_signature(), None);
        assert_eq!(
            verify(
                &unsigned,
                unsigned.validity_predicates(),
                unsigned.validity_signature().as_ref(),
            ),
            Err(ValidityAuthorizationError::MissingSignature)
        );
    }

    #[test]
    fn rejects_missing_wrong_sender_and_noncanonical_signatures() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        assert_eq!(
            verify(&tx, &predicates, None),
            Err(ValidityAuthorizationError::MissingSignature)
        );
        let wrong_signature = sign(&PrivateKeySigner::random(), &tx, &predicates);
        assert_eq!(
            verify(&tx, &predicates, Some(&wrong_signature)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
        let signature = sign(&signer, &tx, &predicates);
        let curve_order: U256 =
            "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141".parse().unwrap();
        let high_s = Signature::new(signature.r(), curve_order - signature.s(), !signature.v());
        assert_eq!(
            verify(&tx, &predicates, Some(&high_s)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
        let malformed = Signature::new(U256::ZERO, U256::ZERO, false);
        assert_eq!(
            verify(&tx, &predicates, Some(&malformed)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
    }

    #[test]
    fn rejects_predicate_addition_removal_and_every_field_mutation() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let mut predicates = predicates();
        predicates.push(ValidityPredicate::Nonce {
            address: Address::repeat_byte(0x44),
            op: ValidityOperator::Equal,
            value: U256::from(7),
        });
        let signature = sign(&signer, &tx, &predicates);
        assert_eq!(verify(&tx, &predicates, Some(&signature)), Ok(()));
        let mut mutations = Vec::new();
        let mut added = predicates.clone();
        added.push(predicates[0].clone());
        mutations.push(added);
        let mut removed = predicates.clone();
        removed.remove(0);
        mutations.push(removed);
        for index in 0..predicates.len() {
            let mut changed = predicates.clone();
            match &mut changed[index] {
                ValidityPredicate::Balance { value, .. }
                | ValidityPredicate::Nonce { value, .. }
                | ValidityPredicate::Storage { value, .. }
                | ValidityPredicate::BlockNumber { value, .. }
                | ValidityPredicate::FlashblockIndex { value, .. } => *value += U256::from(1),
            }
            mutations.push(changed);
            let mut changed = predicates.clone();
            match &mut changed[index] {
                ValidityPredicate::Balance { op, .. }
                | ValidityPredicate::Nonce { op, .. }
                | ValidityPredicate::Storage { op, .. }
                | ValidityPredicate::BlockNumber { op, .. }
                | ValidityPredicate::FlashblockIndex { op, .. } => *op = ValidityOperator::NotEqual,
            }
            mutations.push(changed);
        }
        for index in [0, 1, 4] {
            let mut changed = predicates.clone();
            match &mut changed[index] {
                ValidityPredicate::Balance { address, .. }
                | ValidityPredicate::Nonce { address, .. }
                | ValidityPredicate::Storage { address, .. } => *address = Address::ZERO,
                _ => unreachable!(),
            }
            mutations.push(changed);
        }
        let mut slot = predicates.clone();
        if let ValidityPredicate::Storage { slot, .. } = &mut slot[1] {
            *slot += U256::from(1);
        }
        mutations.push(slot);
        let mut mask = predicates.clone();
        if let ValidityPredicate::Storage { mask, .. } = &mut mask[1] {
            *mask = U256::MAX;
        }
        mutations.push(mask);
        let mut kind = predicates;
        kind[2] = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(100),
        };
        mutations.push(kind);

        for changed in mutations {
            assert_eq!(
                verify(&tx, &changed, Some(&signature)),
                Err(ValidityAuthorizationError::InvalidSignature)
            );
        }
    }

    #[test]
    fn rejects_cross_transaction_and_cross_chain_replay_and_spoofed_wire_sender() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = sign(&signer, &tx, &predicates);
        for other in [transaction(&signer, 8453, 1), transaction(&signer, 84532, 0)] {
            assert_eq!(
                verify(&other, &predicates, Some(&signature)),
                Err(ValidityAuthorizationError::InvalidSignature)
            );
        }
        let wrong_domain = signer
            .sign_hash_sync(&ValidityAuthorization::signing_hash(84532, *tx.hash(), &predicates))
            .unwrap();
        assert_eq!(
            verify(&tx, &predicates, Some(&wrong_domain)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
        let forged = BasePooledTransaction::new(
            alloy_consensus::transaction::Recovered::new_unchecked(
                tx.clone_into_consensus().into_inner(),
                Address::ZERO,
            ),
            tx.encoded_2718().len(),
        );
        assert_eq!(
            verify(&forged, &predicates, Some(&signature)),
            Err(ValidityAuthorizationError::SenderMismatch)
        );
    }

    #[rstest::rstest]
    #[case::legacy_unsigned(ValiditySignatureMode::Off, false, Ok(()))]
    #[case::legacy_signed(ValiditySignatureMode::Off, true, Ok(()))]
    #[case::optional_unsigned(ValiditySignatureMode::VerifyIfPresent, false, Ok(()))]
    #[case::optional_signed(ValiditySignatureMode::VerifyIfPresent, true, Ok(()))]
    #[case::required_unsigned(
        ValiditySignatureMode::Required,
        false,
        Err(ValidityAuthorizationError::MissingSignature)
    )]
    #[case::required_signed(ValiditySignatureMode::Required, true, Ok(()))]
    fn staged_policy_authorizes_sidecars(
        #[case] mode: ValiditySignatureMode,
        #[case] signed: bool,
        #[case] expected: Result<(), ValidityAuthorizationError>,
    ) {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = signed.then(|| sign(&signer, &tx, &predicates));
        let result = ValidityAuthorization::validate_recovered(
            &tx,
            TransactionValidity { validity: predicates, validity_signature: signature },
            mode,
        )
        .map(|_| ());
        assert_eq!(result, expected);
    }

    #[rstest::rstest]
    #[case::raw_ingress(true)]
    #[case::builder_wire(false)]
    fn off_preserves_unverified_signatures_at_both_boundaries(#[case] recovered: bool) {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = sign(&signer, &tx, &predicates);
        let curve_order: U256 =
            "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141".parse().unwrap();
        let signatures = [
            sign(&PrivateKeySigner::random(), &tx, &predicates),
            Signature::new(U256::ZERO, U256::ZERO, false),
            Signature::new(signature.r(), curve_order - signature.s(), !signature.v()),
        ];
        for signature in signatures {
            let sidecar = TransactionValidity {
                validity: predicates.clone(),
                validity_signature: Some(signature),
            };
            let validated = if recovered {
                ValidityAuthorization::validate_recovered(&tx, sidecar, ValiditySignatureMode::Off)
            } else {
                ValidityAuthorization::validate(&tx, sidecar, ValiditySignatureMode::Off)
            }
            .unwrap();
            let attached = tx.clone().with_validity(validated).unwrap();
            assert_eq!(attached.validity_signature(), Some(signature));
            assert_eq!(attached.validity_predicates().len(), predicates.len());
        }
    }

    #[rstest::rstest]
    #[case::optional(ValiditySignatureMode::VerifyIfPresent)]
    #[case::required(ValiditySignatureMode::Required)]
    fn optional_verification_never_accepts_a_bad_signature(#[case] mode: ValiditySignatureMode) {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = sign(&PrivateKeySigner::random(), &tx, &predicates);
        assert_eq!(
            ValidityAuthorization::validate_recovered(
                &tx,
                TransactionValidity { validity: predicates, validity_signature: Some(signature) },
                mode
            )
            .map(|_| ()),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
    }

    #[test]
    fn a_validated_sidecar_cannot_be_attached_to_another_transaction() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = sign(&signer, &tx, &predicates);
        let validated = ValidityAuthorization::validate_recovered(
            &tx,
            TransactionValidity { validity: predicates, validity_signature: Some(signature) },
            ValiditySignatureMode::Required,
        )
        .unwrap();
        let other = transaction(&signer, 8453, 1);
        assert_eq!(
            other.with_validity(validated).unwrap_err(),
            ValidityAuthorizationError::TransactionMismatch
        );
    }

    #[test]
    fn signing_is_permutation_independent_but_keeps_duplicate_multiplicity() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let mut predicates = predicates();
        let signature = sign(&signer, &tx, &predicates);
        predicates.reverse();
        assert_eq!(verify(&tx, &predicates, Some(&signature)), Ok(()));
        predicates.rotate_left(1);
        assert_eq!(verify(&tx, &predicates, Some(&signature)), Ok(()));
        predicates.push(predicates[0].clone());
        assert_eq!(
            verify(&tx, &predicates, Some(&signature)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
    }

    #[test]
    fn wire_discriminants_are_pinned_by_the_wallet_contract() {
        assert_eq!(
            [
                ValidityPredicateKind::Balance as u8,
                ValidityPredicateKind::Storage as u8,
                ValidityPredicateKind::BlockNumber as u8,
                ValidityPredicateKind::FlashblockIndex as u8,
                ValidityPredicateKind::Nonce as u8,
            ],
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            [
                ValidityOperator::LessThan as u8,
                ValidityOperator::LessThanOrEqual as u8,
                ValidityOperator::Equal as u8,
                ValidityOperator::NotEqual as u8,
                ValidityOperator::GreaterThan as u8,
                ValidityOperator::GreaterThanOrEqual as u8,
            ],
            [0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn rejects_predicates_for_transactions_without_a_chain_domain() {
        let deposit: BaseTransactionSigned = TxDeposit::default().into();
        let tx = BasePooledTransaction::new(
            alloy_consensus::transaction::Recovered::new_unchecked(deposit, Address::ZERO),
            0,
        );
        let signature = Signature::new(U256::from(1), U256::from(1), false);
        assert_eq!(
            verify(&tx, &predicates(), Some(&signature)),
            Err(ValidityAuthorizationError::MissingChainId)
        );
    }
}
