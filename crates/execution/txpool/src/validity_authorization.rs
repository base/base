//! Sender authorization for off-chain validity predicates.

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256, Signature, U256};
use alloy_sol_types::{SolStruct, eip712_domain, sol};
use reth_transaction_pool::PoolTransaction;

use crate::{BasePooledTransaction, ValidityOperator, ValidityPredicate};

sol! {
    /// EIP-712 encoding of one validity predicate. Unused fields are zero.
    #[derive(Debug)]
    struct ValidityPredicateData {
        /// Balance = 0, storage = 1, block number = 2, flashblock index = 3.
        uint8 kind;
        /// Less = 0, less/equal = 1, equal = 2, not equal = 3, greater = 4, greater/equal = 5.
        uint8 operator;
        /// Account read by a balance or storage predicate.
        address account;
        /// Storage slot; zero for other predicates.
        uint256 slot;
        /// Storage mask; zero for other predicates.
        uint256 mask;
        /// Right-hand comparison value.
        uint256 value;
    }

    /// EIP-712 authorization binding predicates to one signed transaction.
    #[derive(Debug)]
    struct ValidityAuthorizationData {
        /// Hash of the signed EIP-2718 transaction envelope.
        bytes32 transactionHash;
        /// Predicates in canonical evaluation order.
        ValidityPredicateData[] validity;
    }
}

/// Failure to authorize a validity sidecar with the transaction sender's key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidityAuthorizationError {
    /// Signed sidecars cannot be accepted while enforcement is disabled.
    #[error("signed validity predicates are disabled")]
    Disabled,
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
    /// The signature is malformed, non-canonical, or belongs to another key.
    #[error("invalid validity signature: must be signed by the transaction sender")]
    InvalidSignature,
}

/// EIP-712 signing and verification for transaction validity sidecars.
#[derive(Debug)]
pub struct ValidityAuthorization;

impl ValidityAuthorization {
    /// Enforces the default-off signed-predicate policy at transaction ingress.
    ///
    /// With enforcement off, legacy unsigned predicates remain supported, but
    /// signed sidecars are rejected rather than silently downgrading their protection.
    pub fn validate(
        tx: &BasePooledTransaction,
        predicates: &[ValidityPredicate],
        signature: Option<&Signature>,
        require_signature: bool,
    ) -> Result<(), ValidityAuthorizationError> {
        if require_signature {
            Self::verify(tx, predicates, signature)
        } else if signature.is_some() {
            Err(ValidityAuthorizationError::Disabled)
        } else {
            Ok(())
        }
    }

    /// Returns the EIP-712 digest a sender must sign to authorize these predicates.
    ///
    /// The domain is `Base Transaction Validity`, version `1`, with the transaction's
    /// chain ID. The message binds the complete signed transaction hash and every
    /// predicate field. Predicates are stable-sorted with
    /// [`ValidityPredicate::sort_batch`] before hashing, so forwarding may put timing
    /// predicates first without invalidating the signature. Relative order within
    /// each evaluation rank remains signed. Omitted storage masks sign as `U256::MAX`.
    pub fn signing_hash(
        chain_id: u64,
        transaction_hash: B256,
        predicates: &[ValidityPredicate],
    ) -> B256 {
        let mut predicates = predicates.to_vec();
        ValidityPredicate::sort_batch(&mut predicates);
        let message = ValidityAuthorizationData {
            transactionHash: transaction_hash,
            validity: predicates.iter().map(Self::predicate_data).collect(),
        };
        message.eip712_signing_hash(&eip712_domain! {
            name: "Base Transaction Validity",
            version: "1",
            chain_id: chain_id,
        })
    }

    /// Converts a predicate to its canonical EIP-712 representation.
    pub const fn predicate_data(predicate: &ValidityPredicate) -> ValidityPredicateData {
        let (kind, account, slot, mask, op, value) = match predicate {
            ValidityPredicate::Balance { address, op, value } => {
                (0, *address, U256::ZERO, U256::ZERO, *op, *value)
            }
            ValidityPredicate::Storage { address, slot, mask, op, value } => {
                (1, *address, *slot, *mask, *op, *value)
            }
            ValidityPredicate::BlockNumber { op, value } => {
                (2, Address::ZERO, U256::ZERO, U256::ZERO, *op, *value)
            }
            ValidityPredicate::FlashblockIndex { op, value } => {
                (3, Address::ZERO, U256::ZERO, U256::ZERO, *op, *value)
            }
        };
        let operator = match op {
            ValidityOperator::LessThan => 0,
            ValidityOperator::LessThanOrEqual => 1,
            ValidityOperator::Equal => 2,
            ValidityOperator::NotEqual => 3,
            ValidityOperator::GreaterThan => 4,
            ValidityOperator::GreaterThanOrEqual => 5,
        };
        ValidityPredicateData { kind, operator, account, slot, mask, value }
    }

    /// Verifies the sidecar against the actual transaction sender, never a trusted
    /// builder-wire sender alone. Plain transactions need no extra signature.
    ///
    /// This supports secp256k1 authorization by the sender address. Contract wallets
    /// and configured EIP-8130 actors without that key cannot use this sidecar; their
    /// transaction authenticator is not an authorization of these separate predicates.
    pub fn verify(
        tx: &BasePooledTransaction,
        predicates: &[ValidityPredicate],
        signature: Option<&Signature>,
    ) -> Result<(), ValidityAuthorizationError> {
        if predicates.is_empty() {
            return if signature.is_none() {
                Ok(())
            } else {
                Err(ValidityAuthorizationError::UnexpectedSignature)
            };
        }
        let signature = signature.ok_or(ValidityAuthorizationError::MissingSignature)?;
        let chain_id = tx.chain_id().ok_or(ValidityAuthorizationError::MissingChainId)?;
        let sender = tx
            .consensus_ref()
            .inner()
            .recover_signer()
            .map_err(|_| ValidityAuthorizationError::InvalidTransactionSender)?;
        if sender != tx.sender() {
            return Err(ValidityAuthorizationError::SenderMismatch);
        }
        // Do not normalize a high-s signature into a valid one: reject it.
        if signature.normalize_s().is_some() {
            return Err(ValidityAuthorizationError::InvalidSignature);
        }
        let digest = Self::signing_hash(chain_id, *tx.hash(), predicates);
        let signer = signature
            .recover_address_from_prehash(&digest)
            .map_err(|_| ValidityAuthorizationError::InvalidSignature)?;
        if signer != sender {
            return Err(ValidityAuthorizationError::InvalidSignature);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::TxKind;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
    use reth_transaction_pool::{
        TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    };

    use super::*;
    use crate::{TransactionValidity, ValidatedTransaction, ValidatedTransactionExtensions};

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
        // chain 8453, hash 0x44...44, all four predicate variants in canonical order.
        let expected: B256 =
            "0x59e764e61b4a14f778650f1f053e388f0bca5dc6b25a342f4f182dfe92b73614".parse().unwrap();
        assert_eq!(
            ValidityAuthorization::signing_hash(8453, B256::repeat_byte(0x44), &predicates()),
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
            ValidityAuthorization::verify(&tx, &predicates(), Some(&signature)),
            Err(ValidityAuthorizationError::MissingChainId),
        );
    }

    #[test]
    fn user_authorization_survives_sorting_and_builder_wire_forwarding() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let validity = predicates();
        let signature = sign(&signer, &tx, &validity);
        assert_eq!(ValidityAuthorization::verify(&tx, &validity, Some(&signature)), Ok(()));
        let tx =
            tx.with_validity(TransactionValidity { validity, validity_signature: Some(signature) });
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
        let inbound = decoded.extensions.apply(inbound).unwrap();
        assert_eq!(inbound.validity_signature(), Some(signature));
        assert_eq!(
            ValidityAuthorization::verify(
                &inbound,
                inbound.validity_predicates(),
                inbound.validity_signature().as_ref(),
            ),
            Ok(())
        );
        // Replacing predicates through the unsigned API must not retain stale authorization.
        let unsigned = inbound.with_validity_predicates(predicates());
        assert_eq!(unsigned.validity_signature(), None);
        assert_eq!(
            ValidityAuthorization::verify(
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
            ValidityAuthorization::verify(&tx, &predicates, None),
            Err(ValidityAuthorizationError::MissingSignature)
        );
        let wrong_signature = sign(&PrivateKeySigner::random(), &tx, &predicates);
        assert_eq!(
            ValidityAuthorization::verify(&tx, &predicates, Some(&wrong_signature)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
        let signature = sign(&signer, &tx, &predicates);
        let curve_order: U256 =
            "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141".parse().unwrap();
        let high_s = Signature::new(signature.r(), curve_order - signature.s(), !signature.v());
        assert_eq!(
            ValidityAuthorization::verify(&tx, &predicates, Some(&high_s)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
        let malformed = Signature::new(U256::ZERO, U256::ZERO, false);
        assert_eq!(
            ValidityAuthorization::verify(&tx, &predicates, Some(&malformed)),
            Err(ValidityAuthorizationError::InvalidSignature)
        );
    }

    #[test]
    fn rejects_predicate_addition_removal_and_every_field_mutation() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = sign(&signer, &tx, &predicates);
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
                | ValidityPredicate::Storage { value, .. }
                | ValidityPredicate::BlockNumber { value, .. }
                | ValidityPredicate::FlashblockIndex { value, .. } => *value += U256::from(1),
            }
            mutations.push(changed);
            let mut changed = predicates.clone();
            match &mut changed[index] {
                ValidityPredicate::Balance { op, .. }
                | ValidityPredicate::Storage { op, .. }
                | ValidityPredicate::BlockNumber { op, .. }
                | ValidityPredicate::FlashblockIndex { op, .. } => *op = ValidityOperator::NotEqual,
            }
            mutations.push(changed);
        }
        for index in [0, 1] {
            let mut changed = predicates.clone();
            match &mut changed[index] {
                ValidityPredicate::Balance { address, .. }
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
        let mut kind = predicates.clone();
        kind[2] = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(100),
        };
        mutations.push(kind);
        let mut reordered = predicates;
        reordered.swap(0, 1);
        mutations.push(reordered);
        for changed in mutations {
            assert_eq!(
                ValidityAuthorization::verify(&tx, &changed, Some(&signature)),
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
                ValidityAuthorization::verify(&other, &predicates, Some(&signature)),
                Err(ValidityAuthorizationError::InvalidSignature)
            );
        }
        let wrong_domain = signer
            .sign_hash_sync(&ValidityAuthorization::signing_hash(84532, *tx.hash(), &predicates))
            .unwrap();
        assert_eq!(
            ValidityAuthorization::verify(&tx, &predicates, Some(&wrong_domain)),
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
            ValidityAuthorization::verify(&forged, &predicates, Some(&signature)),
            Err(ValidityAuthorizationError::SenderMismatch)
        );
    }

    #[test]
    fn flag_off_preserves_unsigned_behavior_and_rejects_signed_sidecars() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(&signer, 8453, 0);
        let predicates = predicates();
        let signature = sign(&signer, &tx, &predicates);
        assert_eq!(ValidityAuthorization::validate(&tx, &predicates, None, false), Ok(()));
        assert_eq!(
            ValidityAuthorization::validate(&tx, &predicates, Some(&signature), false),
            Err(ValidityAuthorizationError::Disabled)
        );
        assert_eq!(
            ValidityAuthorization::validate(&tx, &predicates, None, true),
            Err(ValidityAuthorizationError::MissingSignature)
        );
        assert_eq!(
            ValidityAuthorization::validate(&tx, &predicates, Some(&signature), true),
            Ok(())
        );
        assert_eq!(ValidityAuthorization::validate(&tx, &[], None, true), Ok(()));
        assert_eq!(
            ValidityAuthorization::validate(&tx, &[], Some(&signature), true),
            Err(ValidityAuthorizationError::UnexpectedSignature)
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
            ValidityAuthorization::verify(&tx, &predicates(), Some(&signature)),
            Err(ValidityAuthorizationError::MissingChainId)
        );
    }
}
