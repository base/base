//! State predicates carried by pooled transactions.

use alloy_primitives::{Address, U256};
use reth_transaction_pool::ValidPoolTransaction;
use revm::Database;

use crate::{BasePooledTransaction, ExtensionError, ValidatedTransactionExtensions};

/// Maximum number of experimental validity predicates carried by one transaction.
pub const MAX_VALIDITY_PREDICATES: usize = 64;

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

/// A declared state condition for a transaction.
///
/// The JSON representation uses a `type` tag and a `params` object, accepting
/// either `balance` or `storage`. A `storage` predicate compares
/// `storage(address, slot) & mask` with `value`; omitted masks default to
/// [`U256::MAX`]. A `balance` predicate has the same comparison fields but
/// does not accept `slot` or `mask`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", content = "params", rename_all = "lowercase", deny_unknown_fields)]
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
}

impl ValidityPredicate {
    /// Returns the default mask for storage predicates.
    #[must_use]
    pub const fn default_mask() -> U256 {
        U256::MAX
    }

    /// Returns whether this predicate holds against the current database state.
    ///
    /// An absent account has a zero balance. Storage values are masked before
    /// comparison. Callers must treat database errors as an inability to verify
    /// the predicate rather than as a successful match.
    pub fn matches_state<DB: Database>(&self, db: &mut DB) -> Result<bool, DB::Error> {
        match self {
            Self::Balance { address, op, value } => {
                let balance = db.basic(*address)?.map_or(U256::ZERO, |account| account.balance);
                Ok(op.matches(balance, *value))
            }
            Self::Storage { address, slot, mask, op, value } => {
                let storage = db.storage(*address, *slot)? & *mask;
                Ok(op.matches(storage, *value))
            }
        }
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

    fn extract(tx: &ValidPoolTransaction<BasePooledTransaction>) -> Self {
        Self { validity: tx.transaction.validity_predicates().to_vec() }
    }

    fn apply(self, tx: BasePooledTransaction) -> Result<BasePooledTransaction, ExtensionError> {
        if self.validity.len() > MAX_VALIDITY_PREDICATES {
            return Err(ExtensionError(format!(
                "too many validity predicates: {} (maximum {MAX_VALIDITY_PREDICATES})",
                self.validity.len()
            )));
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

        assert!(balance.matches_state(&mut db).unwrap());
        assert!(storage.matches_state(&mut db).unwrap());
    }

    #[test]
    fn absent_accounts_have_zero_balance() {
        let predicate = ValidityPredicate::Balance {
            address: Address::repeat_byte(0x11),
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };

        assert!(predicate.matches_state(&mut InMemoryDB::default()).unwrap());
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
    fn apply_rejects_too_many_predicates() {
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
        let predicate = ValidityPredicate::Balance {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let extension =
            TransactionValidity { validity: vec![predicate; MAX_VALIDITY_PREDICATES + 1] };

        let error = extension.apply(transaction).unwrap_err();

        assert!(error.to_string().contains("too many validity predicates"));
    }
}
