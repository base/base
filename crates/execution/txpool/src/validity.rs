//! State predicates carried by pooled transactions.

use alloy_primitives::{Address, U256};
use reth_transaction_pool::ValidPoolTransaction;
use revm::Database;

use crate::{BasePooledTransaction, ExtensionError, ValidatedTransactionExtensions};

/// Default maximum number of experimental validity predicates carried by one transaction.
pub const DEFAULT_MAX_VALIDITY_PREDICATES: usize = 64;

/// Error returned when a batch of validity predicates fails ingress validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidityPredicateError {
    /// The submission carried no predicates.
    ///
    /// A predicate-less submission has no advanced semantics to enforce, so it is
    /// rejected rather than treated as a plain private transaction.
    #[error("validity predicates must not be empty")]
    Empty,
    /// The submission carried more predicates than the configured maximum.
    #[error("too many validity predicates: {count} (maximum {max})")]
    TooMany {
        /// Number of predicates supplied.
        count: usize,
        /// Maximum number of predicates permitted.
        max: usize,
    },
    /// A storage predicate's comparison value has bits set outside its mask.
    ///
    /// Because the loaded storage value is masked before comparison, bits set in
    /// `value` outside `mask` could never match and indicate a malformed request.
    /// `index` is the position of the offending predicate within the batch.
    #[error("storage predicate at index {index} has value bits set outside its mask")]
    StorageValueOutsideMask {
        /// Position of the offending predicate within the batch.
        index: usize,
    },
}

/// A comparison used by a [`ValidityPredicate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ValidityOperator {
    /// Less than.
    #[serde(rename = "<")]
    LessThan,
    /// Less than or equal to.
    #[serde(rename = "<=")]
    LessThanOrEqual,
    /// Equal to.
    #[serde(rename = "=")]
    Equal,
    /// Not equal to.
    #[serde(rename = "!=")]
    NotEqual,
    /// Greater than.
    #[serde(rename = ">")]
    GreaterThan,
    /// Greater than or equal to.
    #[serde(rename = ">=")]
    GreaterThanOrEqual,
}

impl ValidityOperator {
    /// Returns whether `left op right` holds.
    #[must_use]
    pub fn matches(self, left: U256, right: U256) -> bool {
        match self {
            Self::LessThan => left < right,
            Self::LessThanOrEqual => left <= right,
            Self::Equal => left == right,
            Self::NotEqual => left != right,
            Self::GreaterThan => left > right,
            Self::GreaterThanOrEqual => left >= right,
        }
    }
}

/// Block-level context evaluated by non-state [`ValidityPredicate`] variants.
///
/// Carries the properties of the block and flashblock currently being built so
/// that predicates such as [`ValidityPredicate::BlockNumber`] and
/// [`ValidityPredicate::FlashblockIndex`] can be checked without reading state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredicateContext {
    /// Number of the block currently being built.
    pub block_number: u64,
    /// Index of the flashblock currently being built.
    pub flashblock_index: u64,
}

/// A declared condition for a transaction.
///
/// The JSON representation uses a `type` tag and a `params` object, accepting
/// `balance`, `storage`, `block_number`, or `flashblock_index`. A `storage`
/// predicate compares `storage(address, slot) & mask` with `value`; omitted
/// masks default to [`U256::MAX`]. A `balance` predicate has the same
/// comparison fields but does not accept `slot` or `mask`. The `block_number`
/// and `flashblock_index` predicates compare the block or flashblock currently
/// being built against `value` and accept only `op` and `value`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", content = "params", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValidityPredicate {
    /// Compares an account balance with a value.
    Balance {
        /// Account whose balance is read.
        address: Address,
        /// Comparison to apply to the account balance.
        op: ValidityOperator,
        /// Right-hand comparison value.
        value: U256,
    },
    /// Compares a masked storage value with a value.
    Storage {
        /// Contract whose storage is read.
        address: Address,
        /// Storage slot to read.
        slot: U256,
        /// Bit mask applied to the loaded storage value.
        #[serde(default = "ValidityPredicate::default_mask")]
        mask: U256,
        /// Comparison to apply to the masked storage value.
        op: ValidityOperator,
        /// Right-hand comparison value.
        value: U256,
    },
    /// Compares the number of the block being built with a value.
    BlockNumber {
        /// Comparison to apply to the block number.
        op: ValidityOperator,
        /// Right-hand comparison value.
        value: U256,
    },
    /// Compares the index of the flashblock being built with a value.
    FlashblockIndex {
        /// Comparison to apply to the flashblock index.
        op: ValidityOperator,
        /// Right-hand comparison value.
        value: U256,
    },
}

impl ValidityPredicate {
    /// Returns the default mask for storage predicates.
    #[must_use]
    pub const fn default_mask() -> U256 {
        U256::MAX
    }

    /// Validates that this predicate's parameters are internally consistent.
    ///
    /// A storage predicate's comparison value must not set any bits outside its
    /// mask, since the loaded value is masked before comparison. `index` is the
    /// predicate's position within its submitted batch and is embedded into any
    /// returned error so callers can report which predicate failed. Returning the
    /// specific [`ValidityPredicateError`] keeps the diagnosis with the predicate
    /// itself, so new failure modes surface as distinct errors instead of
    /// collapsing into a single caller-assigned variant.
    pub fn validate_params(&self, index: usize) -> Result<(), ValidityPredicateError> {
        if let Self::Storage { mask, value, .. } = self
            && (*value & !*mask) != U256::ZERO
        {
            return Err(ValidityPredicateError::StorageValueOutsideMask { index });
        }
        Ok(())
    }

    /// Validates a batch of predicates submitted at ingress.
    ///
    /// Rejects an empty batch, a batch larger than `max`, and any predicate
    /// whose parameters are internally inconsistent.
    pub fn validate_batch(predicates: &[Self], max: usize) -> Result<(), ValidityPredicateError> {
        if predicates.is_empty() {
            return Err(ValidityPredicateError::Empty);
        }
        if predicates.len() > max {
            return Err(ValidityPredicateError::TooMany { count: predicates.len(), max });
        }
        for (index, predicate) in predicates.iter().enumerate() {
            predicate.validate_params(index)?;
        }
        Ok(())
    }

    /// Returns whether this predicate holds against the current build.
    ///
    /// State-reading variants ([`Self::Balance`], [`Self::Storage`]) query
    /// `db`; block-level variants ([`Self::BlockNumber`],
    /// [`Self::FlashblockIndex`]) read `context` instead. An absent account has
    /// a zero balance. Storage values are masked before comparison. Callers must
    /// treat database errors as an inability to verify the predicate rather than
    /// as a successful match.
    pub fn matches<DB: Database>(
        &self,
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<bool, DB::Error> {
        match self {
            Self::Balance { address, op, value } => {
                let balance = db.basic(*address)?.map_or(U256::ZERO, |account| account.balance);
                Ok(op.matches(balance, *value))
            }
            Self::Storage { address, slot, mask, op, value } => {
                let storage = db.storage(*address, *slot)? & *mask;
                Ok(op.matches(storage, *value))
            }
            Self::BlockNumber { op, value } => {
                Ok(op.matches(U256::from(context.block_number), *value))
            }
            Self::FlashblockIndex { op, value } => {
                Ok(op.matches(U256::from(context.flashblock_index), *value))
            }
        }
    }

    /// Returns whether these predicates can no longer be satisfied at any build
    /// position at or after `context`.
    ///
    /// Build position advances monotonically: `block_number` strictly increases
    /// across blocks and `flashblock_index` increases from zero within a block.
    /// A [`Self::BlockNumber`] or [`Self::FlashblockIndex`] predicate whose upper
    /// bound the build has already passed can therefore never hold again, so the
    /// transaction is permanently ineligible and should be evicted rather than
    /// parked for a later rescan. State predicates ([`Self::Balance`],
    /// [`Self::Storage`]) are recoverable and never make a batch expired.
    ///
    /// The check is conservative — it reports `true` only when expiry is
    /// provable from upper-bound comparisons (`<`, `<=`, `=`), so any shape it
    /// does not recognize simply parks as before rather than being wrongly
    /// discarded.
    #[must_use]
    pub fn is_batch_expired(predicates: &[Self], context: &PredicateContext) -> bool {
        let current_block = U256::from(context.block_number);
        let current_flashblock = U256::from(context.flashblock_index);

        // Tightest inclusive upper bound implied by each monotonic target.
        // `None` means unbounded.
        let mut block_upper: Option<U256> = None;
        let mut flashblock_upper: Option<U256> = None;

        for predicate in predicates {
            let (op, value, upper) = match predicate {
                Self::BlockNumber { op, value } => (op, value, &mut block_upper),
                Self::FlashblockIndex { op, value } => (op, value, &mut flashblock_upper),
                // State predicates are recoverable and never expire a batch.
                Self::Balance { .. } | Self::Storage { .. } => continue,
            };
            // Only `<`, `<=`, `=` cap a value from above; `!=`, `>`, `>=` do not.
            let candidate = match op {
                ValidityOperator::LessThan => match value.checked_sub(U256::from(1)) {
                    Some(max) => max,
                    // `< 0` can never hold at any position — block number and
                    // flashblock index are both non-negative — so the batch is
                    // permanently expired.
                    None => return true,
                },
                ValidityOperator::LessThanOrEqual | ValidityOperator::Equal => *value,
                ValidityOperator::NotEqual
                | ValidityOperator::GreaterThan
                | ValidityOperator::GreaterThanOrEqual => continue,
            };
            *upper = Some((*upper).map_or(candidate, |current| current.min(candidate)));
        }

        // No block at or after the current one can satisfy the block predicates.
        if block_upper.is_some_and(|max| max < current_block) {
            return true;
        }
        // The flashblock index resets each block, so a passed flashblock bound is
        // terminal only when no later block is allowed either.
        let pinned_to_current_block = block_upper == Some(current_block);
        pinned_to_current_block && flashblock_upper.is_some_and(|max| max < current_flashblock)
    }

    /// Returns the inclusive last block at which these predicates can still be
    /// satisfied, when the `block_number` predicates impose a finite upper bound.
    ///
    /// Returns `Some(0)` when the block predicates are already unsatisfiable at
    /// any block (drop as soon as the chain advances), and `None` when no
    /// `block_number` upper bound applies or the bound exceeds [`u64::MAX`]. This
    /// is the pool-side, block-granular projection of [`Self::is_batch_expired`];
    /// the finer flashblock deadline is enforced only by the builder.
    #[must_use]
    pub fn block_expiry_bound(predicates: &[Self]) -> Option<u64> {
        let mut upper: Option<U256> = None;
        for predicate in predicates {
            let Self::BlockNumber { op, value } = predicate else { continue };
            let candidate = match op {
                ValidityOperator::LessThan => match value.checked_sub(U256::from(1)) {
                    Some(max) => max,
                    // `< 0` can never hold, so the batch is permanently expired.
                    None => return Some(0),
                },
                ValidityOperator::LessThanOrEqual | ValidityOperator::Equal => *value,
                ValidityOperator::NotEqual
                | ValidityOperator::GreaterThan
                | ValidityOperator::GreaterThanOrEqual => continue,
            };
            upper = Some(upper.map_or(candidate, |current| current.min(candidate)));
        }
        upper.and_then(|bound| u64::try_from(bound).ok())
    }
}

/// Experimental validity predicates carried with a validated transaction.
///
/// The builder transport preserves these predicates but does not currently
/// evaluate them during block construction.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TransactionValidity {
    /// Predicates intended to control when the transaction is valid for inclusion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validity: Vec<ValidityPredicate>,
}

impl ValidatedTransactionExtensions<BasePooledTransaction> for TransactionValidity {
    fn is_empty(&self) -> bool {
        self.validity.is_empty()
    }

    fn validate(&self, max_items: usize) -> Result<(), ExtensionError> {
        if self.validity.len() > max_items {
            return Err(ExtensionError(format!(
                "too many validity predicates: {} (maximum {max_items})",
                self.validity.len()
            )));
        }
        Ok(())
    }

    fn extract(tx: &ValidPoolTransaction<BasePooledTransaction>) -> Self {
        Self { validity: tx.transaction.validity_predicates().to_vec() }
    }

    /// Applies the predicates to the builder-inbound transaction.
    ///
    /// This is the builder RPC ingress path, distinct from the mempool node's
    /// `base_sendRawTransactionValidity`. The count bound is enforced separately
    /// by [`Self::validate`]; here each predicate's parameters are re-checked as
    /// defense-in-depth against a misbehaving upstream. Unlike the mempool
    /// ingress, an empty predicate set is not rejected: the builder legitimately
    /// receives ordinary transactions that carry no predicates.
    fn apply(self, tx: BasePooledTransaction) -> Result<BasePooledTransaction, ExtensionError> {
        for (index, predicate) in self.validity.iter().enumerate() {
            predicate.validate_params(index).map_err(|e| ExtensionError(e.to_string()))?;
        }
        Ok(tx.with_validity_predicates(self.validity))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::TxKind;
    use base_common_consensus::{BaseTransactionSigned, TxDeposit};
    use revm::{database::InMemoryDB, state::AccountInfo};
    use serde_json::json;

    use super::*;

    /// A predicate context with arbitrary block and flashblock coordinates.
    fn test_context() -> PredicateContext {
        PredicateContext { block_number: 100, flashblock_index: 3 }
    }

    #[test]
    fn deserializes_storage_predicate_with_default_mask() {
        let predicate: ValidityPredicate = serde_json::from_str(
            r#"{"type":"storage","params":{"address":"0x1111111111111111111111111111111111111111","slot":"0x7","op":"=","value":"0x12ab"}}"#,
        )
        .unwrap();

        assert_eq!(
            predicate,
            ValidityPredicate::Storage {
                address: Address::repeat_byte(0x11),
                slot: U256::from(7),
                mask: U256::MAX,
                op: ValidityOperator::Equal,
                value: U256::from(0x12ab),
            }
        );
    }

    #[test]
    fn predicate_json_rejects_fields_not_supported_by_its_type() {
        let balance_with_slot = r#"{"type":"balance","params":{"address":"0x1111111111111111111111111111111111111111","slot":"0x7","op":"=","value":"0x0"}}"#;
        let storage_without_slot = r#"{"type":"storage","params":{"address":"0x1111111111111111111111111111111111111111","op":"=","value":"0x0"}}"#;

        assert!(serde_json::from_str::<ValidityPredicate>(balance_with_slot).is_err());
        assert!(serde_json::from_str::<ValidityPredicate>(storage_without_slot).is_err());
    }

    #[test]
    fn predicate_json_rejects_flattened_parameters() {
        let flattened = r#"{"type":"balance","address":"0x1111111111111111111111111111111111111111","op":"=","value":"0x0"}"#;

        assert!(serde_json::from_str::<ValidityPredicate>(flattened).is_err());
    }

    #[test]
    fn serializes_predicate_parameters_under_params() {
        let predicate = ValidityPredicate::Balance {
            address: Address::repeat_byte(0x11),
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(1),
        };

        assert_eq!(
            serde_json::to_value(predicate).unwrap(),
            json!({
                "type": "balance",
                "params": {
                    "address": "0x1111111111111111111111111111111111111111",
                    "op": ">=",
                    "value": "0x1",
                },
            })
        );
    }

    #[test]
    fn deserializes_every_comparison_operator() {
        for (op, expected) in [
            ("<", ValidityOperator::LessThan),
            ("<=", ValidityOperator::LessThanOrEqual),
            ("=", ValidityOperator::Equal),
            ("!=", ValidityOperator::NotEqual),
            (">", ValidityOperator::GreaterThan),
            (">=", ValidityOperator::GreaterThanOrEqual),
        ] {
            let predicate: ValidityPredicate = serde_json::from_str(&format!(
                r#"{{"type":"balance","params":{{"address":"0x1111111111111111111111111111111111111111","op":"{op}","value":"0x0"}}}}"#
            ))
            .unwrap();

            assert_eq!(
                predicate,
                ValidityPredicate::Balance {
                    address: Address::repeat_byte(0x11),
                    op: expected,
                    value: U256::ZERO,
                }
            );
        }
    }

    #[test]
    fn comparison_operators_match_expected_values() {
        for (op, expected) in [
            (ValidityOperator::LessThan, true),
            (ValidityOperator::LessThanOrEqual, true),
            (ValidityOperator::Equal, false),
            (ValidityOperator::NotEqual, true),
            (ValidityOperator::GreaterThan, false),
            (ValidityOperator::GreaterThanOrEqual, false),
        ] {
            assert_eq!(op.matches(U256::from(10), U256::from(11)), expected);
        }
    }

    #[test]
    fn predicates_match_current_state() {
        let address = Address::repeat_byte(0x11);
        let slot = U256::from(7);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            address,
            AccountInfo { balance: U256::from(10), ..Default::default() },
        );
        db.insert_account_storage(address, slot, U256::from(0xabcd)).unwrap();

        let balance = ValidityPredicate::Balance {
            address,
            op: ValidityOperator::GreaterThan,
            value: U256::from(9),
        };
        let storage = ValidityPredicate::Storage {
            address,
            slot,
            mask: U256::from(0xff),
            op: ValidityOperator::Equal,
            value: U256::from(0xcd),
        };

        assert!(balance.matches(&mut db, &test_context()).unwrap());
        assert!(storage.matches(&mut db, &test_context()).unwrap());
    }

    #[test]
    fn absent_accounts_have_zero_balance() {
        let predicate = ValidityPredicate::Balance {
            address: Address::repeat_byte(0x11),
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };

        assert!(predicate.matches(&mut InMemoryDB::default(), &test_context()).unwrap());
    }

    #[test]
    fn empty_payload_is_omitted() {
        assert_eq!(serde_json::to_value(TransactionValidity::default()).unwrap(), json!({}));
    }

    #[test]
    fn predicates_round_trip() {
        let value = json!({
            "validity": [{
                "type": "balance",
                "params": {
                    "address": "0x1111111111111111111111111111111111111111",
                    "op": "=",
                    "value": "0x2"
                }
            }]
        });
        let payload: TransactionValidity = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(
            payload.validity,
            vec![ValidityPredicate::Balance {
                address: Address::repeat_byte(0x11),
                op: ValidityOperator::Equal,
                value: U256::from(2),
            }]
        );
        assert_eq!(serde_json::to_value(payload).unwrap(), value);
    }

    #[test]
    fn apply_attaches_validity_predicates_to_pooled_transaction() {
        let signed: BaseTransactionSigned = TxDeposit {
            source_hash: Default::default(),
            from: Address::ZERO,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
        }
        .into();
        let encoded_length = signed.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(signed, Address::ZERO),
            encoded_length,
        );
        let expected = vec![ValidityPredicate::Storage {
            address: Address::repeat_byte(0x11),
            slot: U256::from(1),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(2),
        }];
        let extension = TransactionValidity { validity: expected.clone() };

        let transaction = extension.apply(transaction).unwrap();

        assert_eq!(transaction.validity_predicates(), expected);
    }

    #[test]
    fn validate_rejects_configured_maximum() {
        let predicate = ValidityPredicate::Balance {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let extension = TransactionValidity { validity: vec![predicate; 3] };

        let error = extension.validate(2).unwrap_err();

        assert!(error.to_string().contains("too many validity predicates"));
        assert!(error.to_string().contains("maximum 2"));
    }

    #[test]
    fn deserializes_block_number_and_flashblock_index_predicates() {
        let block_number: ValidityPredicate =
            serde_json::from_str(r#"{"type":"block_number","params":{"op":">=","value":"0x64"}}"#)
                .unwrap();
        let flashblock_index: ValidityPredicate = serde_json::from_str(
            r#"{"type":"flashblock_index","params":{"op":"<","value":"0x5"}}"#,
        )
        .unwrap();

        assert_eq!(
            block_number,
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(100),
            }
        );
        assert_eq!(
            flashblock_index,
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThan,
                value: U256::from(5),
            }
        );
    }

    #[test]
    fn block_number_and_flashblock_index_predicates_reject_state_fields() {
        let block_number_with_address = r#"{"type":"block_number","params":{"address":"0x1111111111111111111111111111111111111111","op":"=","value":"0x0"}}"#;
        let flashblock_index_with_slot =
            r#"{"type":"flashblock_index","params":{"slot":"0x1","op":"=","value":"0x0"}}"#;

        assert!(serde_json::from_str::<ValidityPredicate>(block_number_with_address).is_err());
        assert!(serde_json::from_str::<ValidityPredicate>(flashblock_index_with_slot).is_err());
    }

    #[test]
    fn block_level_predicates_match_context_without_reading_state() {
        let context = PredicateContext { block_number: 100, flashblock_index: 3 };
        let mut db = InMemoryDB::default();

        let block_number = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(100),
        };
        let block_number_too_low = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThan,
            value: U256::from(100),
        };
        let flashblock_index = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::Equal,
            value: U256::from(3),
        };
        let flashblock_index_mismatch = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::GreaterThan,
            value: U256::from(3),
        };

        assert!(block_number.matches(&mut db, &context).unwrap());
        assert!(!block_number_too_low.matches(&mut db, &context).unwrap());
        assert!(flashblock_index.matches(&mut db, &context).unwrap());
        assert!(!flashblock_index_mismatch.matches(&mut db, &context).unwrap());
    }

    #[test]
    fn flashblock_index_predicate_round_trips() {
        let predicate = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(7),
        };

        assert_eq!(
            serde_json::to_value(&predicate).unwrap(),
            json!({
                "type": "flashblock_index",
                "params": {
                    "op": "<=",
                    "value": "0x7",
                },
            })
        );
        assert_eq!(
            serde_json::from_value::<ValidityPredicate>(serde_json::to_value(&predicate).unwrap())
                .unwrap(),
            predicate
        );
    }

    #[test]
    fn block_number_predicate_round_trips() {
        let predicate = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(0x1234),
        };

        assert_eq!(
            serde_json::to_value(&predicate).unwrap(),
            json!({
                "type": "block_number",
                "params": {
                    "op": ">=",
                    "value": "0x1234",
                },
            })
        );
        assert_eq!(
            serde_json::from_value::<ValidityPredicate>(serde_json::to_value(&predicate).unwrap())
                .unwrap(),
            predicate
        );
    }

    #[test]
    fn apply_rejects_storage_value_outside_mask() {
        let signed: BaseTransactionSigned = TxDeposit {
            source_hash: Default::default(),
            from: Address::ZERO,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
        }
        .into();
        let encoded_length = signed.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(signed, Address::ZERO),
            encoded_length,
        );
        let extension = TransactionValidity {
            validity: vec![ValidityPredicate::Storage {
                address: Address::repeat_byte(0x22),
                slot: U256::from(7),
                mask: U256::from(0xff),
                op: ValidityOperator::Equal,
                value: U256::from(0x100),
            }],
        };

        let error = extension.apply(transaction).unwrap_err();

        assert!(error.to_string().contains("outside its mask"));
    }

    #[test]
    fn apply_accepts_empty_predicates() {
        let signed: BaseTransactionSigned = TxDeposit {
            source_hash: Default::default(),
            from: Address::ZERO,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
        }
        .into();
        let encoded_length = signed.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(signed, Address::ZERO),
            encoded_length,
        );

        // The builder path legitimately receives ordinary transactions with no
        // predicates; unlike the mempool ingress, `apply` must not reject them.
        let transaction = TransactionValidity::default().apply(transaction).unwrap();

        assert!(transaction.validity_predicates().is_empty());
    }

    #[test]
    fn validate_params_accepts_storage_value_within_mask() {
        let predicate = ValidityPredicate::Storage {
            address: Address::repeat_byte(0x11),
            slot: U256::from(1),
            mask: U256::from(0xff),
            op: ValidityOperator::Equal,
            value: U256::from(0xab),
        };

        assert_eq!(predicate.validate_params(0), Ok(()));
    }

    #[test]
    fn validate_params_rejects_storage_value_outside_mask_reporting_its_index() {
        let predicate = ValidityPredicate::Storage {
            address: Address::repeat_byte(0x11),
            slot: U256::from(1),
            mask: U256::from(0xff),
            op: ValidityOperator::Equal,
            value: U256::from(0x1ff),
        };

        assert_eq!(
            predicate.validate_params(3),
            Err(ValidityPredicateError::StorageValueOutsideMask { index: 3 })
        );
    }

    #[test]
    fn validate_params_ignores_mask_for_non_storage_predicates() {
        let predicate = ValidityPredicate::Balance {
            address: Address::repeat_byte(0x11),
            op: ValidityOperator::Equal,
            value: U256::MAX,
        };

        assert_eq!(predicate.validate_params(0), Ok(()));
    }

    #[test]
    fn validate_batch_rejects_empty() {
        assert_eq!(
            ValidityPredicate::validate_batch(&[], DEFAULT_MAX_VALIDITY_PREDICATES),
            Err(ValidityPredicateError::Empty)
        );
    }

    #[test]
    fn validate_batch_rejects_too_many() {
        let predicate = ValidityPredicate::Balance {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let predicates = vec![predicate; DEFAULT_MAX_VALIDITY_PREDICATES + 1];

        assert_eq!(
            ValidityPredicate::validate_batch(&predicates, DEFAULT_MAX_VALIDITY_PREDICATES),
            Err(ValidityPredicateError::TooMany {
                count: DEFAULT_MAX_VALIDITY_PREDICATES + 1,
                max: DEFAULT_MAX_VALIDITY_PREDICATES,
            })
        );
    }

    #[test]
    fn validate_batch_accepts_valid_predicates() {
        let predicates = vec![
            ValidityPredicate::Balance {
                address: Address::repeat_byte(0x11),
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(1),
            },
            ValidityPredicate::Storage {
                address: Address::repeat_byte(0x22),
                slot: U256::from(7),
                mask: U256::from(0xff),
                op: ValidityOperator::Equal,
                value: U256::from(0xcd),
            },
        ];

        assert_eq!(
            ValidityPredicate::validate_batch(&predicates, DEFAULT_MAX_VALIDITY_PREDICATES),
            Ok(())
        );
    }

    #[test]
    fn validate_batch_rejects_malformed_predicate_reporting_its_index() {
        let valid = ValidityPredicate::Balance {
            address: Address::repeat_byte(0x11),
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let malformed = ValidityPredicate::Storage {
            address: Address::repeat_byte(0x22),
            slot: U256::from(7),
            mask: U256::from(0xff),
            op: ValidityOperator::Equal,
            value: U256::from(0x100),
        };
        let predicates = vec![valid, malformed];

        assert_eq!(
            ValidityPredicate::validate_batch(&predicates, DEFAULT_MAX_VALIDITY_PREDICATES),
            Err(ValidityPredicateError::StorageValueOutsideMask { index: 1 })
        );
    }

    /// Builds a context at the given build position.
    fn context_at(block_number: u64, flashblock_index: u64) -> PredicateContext {
        PredicateContext { block_number, flashblock_index }
    }

    /// A block-number predicate with the given operator and value.
    fn block_number(op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::BlockNumber { op, value: U256::from(value) }
    }

    /// A flashblock-index predicate with the given operator and value.
    fn flashblock_index(op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::FlashblockIndex { op, value: U256::from(value) }
    }

    #[test]
    fn block_number_upper_bound_expires_once_passed() {
        for (op, deadline) in
            [(ValidityOperator::LessThanOrEqual, 100), (ValidityOperator::Equal, 100)]
        {
            let predicates = [block_number(op, deadline)];
            assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(100, 0)));
            assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(101, 0)));
        }

        // `<` is exclusive, so it expires one block earlier.
        let predicates = [block_number(ValidityOperator::LessThan, 100)];
        assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(99, 0)));
        assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(100, 0)));
    }

    #[test]
    fn block_number_lower_bounds_and_inequality_never_expire() {
        for op in [
            ValidityOperator::GreaterThan,
            ValidityOperator::GreaterThanOrEqual,
            ValidityOperator::NotEqual,
        ] {
            let predicates = [block_number(op, 100)];
            assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(1_000, 0)));
        }
    }

    #[test]
    fn flashblock_index_alone_never_expires() {
        // The flashblock index resets each block, so a future block can still
        // satisfy an upper-bounded flashblock predicate.
        let predicates = [flashblock_index(ValidityOperator::LessThanOrEqual, 2)];
        assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(100, 9)));
    }

    #[test]
    fn composite_block_and_flashblock_deadline_expires_within_the_pinned_block() {
        // Pinned to block 100, valid through flashblock index 2.
        let predicates = [
            block_number(ValidityOperator::Equal, 100),
            flashblock_index(ValidityOperator::LessThanOrEqual, 2),
        ];
        // Still satisfiable up to and including (100, 2).
        assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(100, 2)));
        // Past the flashblock bound within the pinned block: terminal.
        assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(100, 3)));
        // Before the pinned block, the flashblock bound is not yet binding.
        assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(99, 9)));
        // After the pinned block: terminal via the block bound.
        assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(101, 0)));
    }

    #[test]
    fn unsatisfiable_upper_bound_expires_immediately() {
        // `block_number < 0` can never hold at any position.
        let predicates = [block_number(ValidityOperator::LessThan, 0)];
        assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(0, 0)));

        // `flashblock_index < 0` likewise, regardless of block bound.
        let predicates = [flashblock_index(ValidityOperator::LessThan, 0)];
        assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(50, 0)));
    }

    #[test]
    fn tightest_block_bound_wins() {
        let predicates = [
            block_number(ValidityOperator::LessThanOrEqual, 200),
            block_number(ValidityOperator::LessThanOrEqual, 100),
        ];
        assert!(!ValidityPredicate::is_batch_expired(&predicates, &context_at(100, 0)));
        assert!(ValidityPredicate::is_batch_expired(&predicates, &context_at(101, 0)));
    }

    #[test]
    fn state_only_and_empty_batches_never_expire() {
        let state_only = [
            ValidityPredicate::Balance {
                address: Address::repeat_byte(0x11),
                op: ValidityOperator::Equal,
                value: U256::from(1),
            },
            ValidityPredicate::Storage {
                address: Address::repeat_byte(0x22),
                slot: U256::from(7),
                mask: U256::MAX,
                op: ValidityOperator::Equal,
                value: U256::from(3),
            },
        ];
        assert!(!ValidityPredicate::is_batch_expired(&state_only, &context_at(10_000, 9)));
        assert!(!ValidityPredicate::is_batch_expired(&[], &context_at(10_000, 9)));
    }

    #[test]
    fn block_expiry_bound_uses_the_inclusive_upper_bound() {
        assert_eq!(
            ValidityPredicate::block_expiry_bound(&[block_number(
                ValidityOperator::LessThanOrEqual,
                100
            )]),
            Some(100)
        );
        assert_eq!(
            ValidityPredicate::block_expiry_bound(&[block_number(ValidityOperator::Equal, 100)]),
            Some(100)
        );
        // `<` is exclusive, so the last valid block is one lower.
        assert_eq!(
            ValidityPredicate::block_expiry_bound(&[block_number(ValidityOperator::LessThan, 100)]),
            Some(99)
        );
    }

    #[test]
    fn block_expiry_bound_ignores_non_upper_bounds() {
        for op in [
            ValidityOperator::GreaterThan,
            ValidityOperator::GreaterThanOrEqual,
            ValidityOperator::NotEqual,
        ] {
            assert_eq!(ValidityPredicate::block_expiry_bound(&[block_number(op, 100)]), None);
        }
        // No block predicate at all.
        assert_eq!(
            ValidityPredicate::block_expiry_bound(&[flashblock_index(
                ValidityOperator::LessThanOrEqual,
                2
            )]),
            None
        );
        assert_eq!(ValidityPredicate::block_expiry_bound(&[]), None);
    }

    #[test]
    fn block_expiry_bound_takes_the_tightest_bound() {
        let predicates = [
            block_number(ValidityOperator::LessThanOrEqual, 200),
            block_number(ValidityOperator::LessThanOrEqual, 100),
        ];
        assert_eq!(ValidityPredicate::block_expiry_bound(&predicates), Some(100));
    }

    #[test]
    fn block_expiry_bound_reports_zero_for_unsatisfiable_bounds() {
        assert_eq!(
            ValidityPredicate::block_expiry_bound(&[block_number(ValidityOperator::LessThan, 0)]),
            Some(0)
        );
    }

    #[test]
    fn block_expiry_bound_is_none_when_bound_exceeds_u64() {
        let predicate = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(u64::MAX) + U256::from(1),
        };
        assert_eq!(ValidityPredicate::block_expiry_bound(&[predicate]), None);
    }
}
