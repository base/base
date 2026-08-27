use alloy_primitives::U256;
use base_execution_txpool::{DEFAULT_MAX_VALIDITY_PREDICATES, ValidityOperator};
use serde::{Deserialize, Serialize};

use super::parsing::parse_address;
use crate::{
    runner::{BlockNumberBound, PredicateAddress, SlotTemplate, ValidityPredicateTemplate},
    utils::{BaselineError, Result},
};

/// Validity-transaction workload configuration.
///
/// A fraction of *senders* route their entire traffic through
/// `base_sendRawTransactionValidity`, carrying the configured predicates.
/// Routing is per sender (not per transaction) so each sender's nonce stream
/// stays on a single submission origin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidityConfig {
    /// Fraction `0.0..=1.0` of senders assigned to the validity path.
    pub ratio: f64,

    /// Predicate templates attached to each validity-bearing transaction.
    ///
    /// Must be non-empty when `ratio > 0`, and must contain at most
    /// [`DEFAULT_MAX_VALIDITY_PREDICATES`] entries.
    pub predicates: Vec<ValidityPredicateConfig>,
}

/// A configured validity predicate template.
///
/// Numeric values, slots, and masks deserialize directly into [`U256`], which
/// accepts hex (`0x`-prefixed), decimal, and other radix-prefixed forms.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValidityPredicateConfig {
    /// Compares an account balance with a value.
    Balance {
        /// Account whose balance is read, resolved per transaction.
        #[serde(default)]
        address: PredicateAddressConfig,
        /// Comparison operator (`<`, `<=`, `=`, `!=`, `>`, `>=`).
        op: String,
        /// Right-hand comparison value.
        value: U256,
    },
    /// Compares a masked storage value with a value.
    Storage {
        /// Contract whose storage is read, resolved per transaction.
        #[serde(default)]
        address: PredicateAddressConfig,
        /// Storage slot to read.
        slot: PredicateSlotConfig,
        /// Optional bit mask; defaults to all ones server-side.
        #[serde(default)]
        mask: Option<U256>,
        /// Comparison operator (`<`, `<=`, `=`, `!=`, `>`, `>=`).
        op: String,
        /// Right-hand comparison value.
        value: U256,
    },
    /// Compares the number of the block being built with a bound.
    ///
    /// Exactly one of `value` (a fixed absolute block number) or `offset` (a
    /// runtime offset resolved to `current_block + offset` at prepare time)
    /// must be set; setting zero or both is a configuration error.
    BlockNumber {
        /// Comparison operator (`<`, `<=`, `=`, `!=`, `>`, `>=`).
        op: String,
        /// Fixed absolute block number. Mutually exclusive with `offset`.
        #[serde(default)]
        value: Option<U256>,
        /// Runtime offset resolved to `current_block + offset` at prepare time.
        /// Mutually exclusive with `value`.
        #[serde(default)]
        offset: Option<U256>,
    },
    /// Compares the index of the flashblock being built with a value.
    FlashblockIndex {
        /// Comparison operator (`<`, `<=`, `=`, `!=`, `>`, `>=`).
        op: String,
        /// Right-hand comparison value.
        value: U256,
    },
}

/// Source for a predicate's address, resolved per transaction at prepare time.
///
/// Written in YAML as a bare string: the keywords `sender` or `recipient`, or
/// an explicit `0x` address literal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PredicateAddressConfig {
    /// The transaction's sender (`from`).
    #[default]
    Sender,
    /// The transaction's recipient (`to`).
    Recipient,
    /// An explicit `0x` address.
    Fixed(String),
}

impl Serialize for PredicateAddressConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let s = match self {
            Self::Sender => "sender",
            Self::Recipient => "recipient",
            Self::Fixed(addr) => addr.as_str(),
        };
        serializer.serialize_str(s)
    }
}

impl<'de> Deserialize<'de> for PredicateAddressConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "sender" => Self::Sender,
            "recipient" => Self::Recipient,
            _ => Self::Fixed(raw),
        })
    }
}

/// Source for a storage slot, resolved per transaction at prepare time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PredicateSlotConfig {
    /// A static storage slot.
    Fixed {
        /// Slot index.
        value: U256,
    },
    /// A Solidity mapping slot `keccak256(key ++ mapping_slot)`, e.g. the
    /// `balanceOf` slot for a given key address.
    Mapping {
        /// Declared position of the mapping in contract storage.
        mapping_slot: U256,
        /// Mapping key address, resolved per transaction.
        #[serde(default)]
        key: PredicateAddressConfig,
    },
}

impl ValidityConfig {
    /// Validates the validity configuration.
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.ratio) {
            return Err(BaselineError::Config("validity.ratio must be between 0.0 and 1.0".into()));
        }
        if self.predicates.len() > DEFAULT_MAX_VALIDITY_PREDICATES {
            return Err(BaselineError::Config(format!(
                "validity.predicates has {} entries, exceeding the maximum of {DEFAULT_MAX_VALIDITY_PREDICATES}",
                self.predicates.len()
            )));
        }
        if self.ratio > 0.0 && self.predicates.is_empty() {
            return Err(BaselineError::Config(
                "validity.predicates must be non-empty when validity.ratio > 0".into(),
            ));
        }
        // Surface parse errors (operators, addresses, values) eagerly.
        for predicate in &self.predicates {
            predicate.to_template()?;
        }
        Ok(())
    }

    /// Converts the configured predicate templates into runtime templates.
    pub fn to_templates(&self) -> Result<Vec<ValidityPredicateTemplate>> {
        self.predicates.iter().map(ValidityPredicateConfig::to_template).collect()
    }
}

impl ValidityPredicateConfig {
    /// Converts this configured predicate into a runtime template, parsing all
    /// literal values and pre-resolving fixed addresses.
    pub fn to_template(&self) -> Result<ValidityPredicateTemplate> {
        match self {
            Self::Balance { address, op, value } => Ok(ValidityPredicateTemplate::Balance {
                address: address.to_template()?,
                op: parse_operator(op)?,
                value: *value,
            }),
            Self::Storage { address, slot, mask, op, value } => {
                Ok(ValidityPredicateTemplate::Storage {
                    address: address.to_template()?,
                    slot: slot.to_template()?,
                    mask: *mask,
                    op: parse_operator(op)?,
                    value: *value,
                })
            }
            Self::BlockNumber { op, value, offset } => {
                let bound = match (value, offset) {
                    (Some(value), None) => BlockNumberBound::Absolute(*value),
                    (None, Some(offset)) => BlockNumberBound::Offset(*offset),
                    _ => {
                        return Err(BaselineError::Config(
                            "block_number predicate requires exactly one of 'value' or 'offset'"
                                .into(),
                        ));
                    }
                };
                Ok(ValidityPredicateTemplate::BlockNumber { op: parse_operator(op)?, bound })
            }
            Self::FlashblockIndex { op, value } => Ok(ValidityPredicateTemplate::FlashblockIndex {
                op: parse_operator(op)?,
                value: *value,
            }),
        }
    }
}

impl PredicateAddressConfig {
    /// Resolves this configured address source into a runtime template.
    pub fn to_template(&self) -> Result<PredicateAddress> {
        match self {
            Self::Sender => Ok(PredicateAddress::Sender),
            Self::Recipient => Ok(PredicateAddress::Recipient),
            Self::Fixed(addr) => {
                Ok(PredicateAddress::Fixed(parse_address(addr, "validity predicate")?))
            }
        }
    }
}

impl PredicateSlotConfig {
    /// Resolves this configured slot source into a runtime template.
    pub fn to_template(&self) -> Result<SlotTemplate> {
        match self {
            Self::Fixed { value } => Ok(SlotTemplate::Fixed(*value)),
            Self::Mapping { mapping_slot, key } => {
                Ok(SlotTemplate::Mapping { mapping_slot: *mapping_slot, key: key.to_template()? })
            }
        }
    }
}

/// Parses a comparison operator symbol into a [`ValidityOperator`].
pub(super) fn parse_operator(s: &str) -> Result<ValidityOperator> {
    match s.trim() {
        "<" => Ok(ValidityOperator::LessThan),
        "<=" => Ok(ValidityOperator::LessThanOrEqual),
        "=" => Ok(ValidityOperator::Equal),
        "!=" => Ok(ValidityOperator::NotEqual),
        ">" => Ok(ValidityOperator::GreaterThan),
        ">=" => Ok(ValidityOperator::GreaterThanOrEqual),
        other => Err(BaselineError::Config(format!(
            "invalid validity operator '{other}', expected one of <, <=, =, !=, >, >="
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_operator_accepts_all_symbols() {
        assert_eq!(parse_operator("<").unwrap(), ValidityOperator::LessThan);
        assert_eq!(parse_operator("<=").unwrap(), ValidityOperator::LessThanOrEqual);
        assert_eq!(parse_operator("=").unwrap(), ValidityOperator::Equal);
        assert_eq!(parse_operator("!=").unwrap(), ValidityOperator::NotEqual);
        assert_eq!(parse_operator(">").unwrap(), ValidityOperator::GreaterThan);
        assert_eq!(parse_operator(">=").unwrap(), ValidityOperator::GreaterThanOrEqual);
    }

    #[test]
    fn parse_operator_rejects_unknown() {
        assert!(parse_operator("==").is_err());
    }

    #[test]
    fn balance_predicate_to_template() {
        let config = ValidityPredicateConfig::Balance {
            address: PredicateAddressConfig::Sender,
            op: ">=".into(),
            value: U256::ZERO,
        };
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::Balance { address, op, value } => {
                assert!(matches!(address, PredicateAddress::Sender));
                assert_eq!(op, ValidityOperator::GreaterThanOrEqual);
                assert_eq!(value, U256::ZERO);
            }
            other => panic!("expected balance template, got {other:?}"),
        }
    }

    #[test]
    fn storage_predicate_with_hex_slot_and_mask() {
        let config = ValidityPredicateConfig::Storage {
            address: PredicateAddressConfig::Fixed(
                "0x1234567890123456789012345678901234567890".into(),
            ),
            slot: PredicateSlotConfig::Fixed { value: U256::from(1u64) },
            mask: Some(U256::from(0xffu64)),
            op: "=".into(),
            value: U256::ZERO,
        };
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::Storage { address, slot, mask, op, value } => {
                assert!(matches!(address, PredicateAddress::Fixed(_)));
                assert!(matches!(slot, SlotTemplate::Fixed(s) if s == U256::from(1u64)));
                assert_eq!(mask, Some(U256::from(0xffu64)));
                assert_eq!(op, ValidityOperator::Equal);
                assert_eq!(value, U256::ZERO);
            }
            other => panic!("expected storage template, got {other:?}"),
        }
    }

    #[test]
    fn mapping_slot_to_template() {
        let config = PredicateSlotConfig::Mapping {
            mapping_slot: U256::ZERO,
            key: PredicateAddressConfig::Sender,
        };
        match config.to_template().unwrap() {
            SlotTemplate::Mapping { mapping_slot, key } => {
                assert_eq!(mapping_slot, U256::ZERO);
                assert!(matches!(key, PredicateAddress::Sender));
            }
            other => panic!("expected mapping template, got {other:?}"),
        }
    }

    #[test]
    fn block_number_predicate_to_template() {
        let config = ValidityPredicateConfig::BlockNumber {
            op: ">=".into(),
            value: Some(U256::from(0x100)),
            offset: None,
        };
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::BlockNumber { op, bound } => {
                assert_eq!(op, ValidityOperator::GreaterThanOrEqual);
                assert_eq!(bound, BlockNumberBound::Absolute(U256::from(0x100)));
            }
            other => panic!("expected block_number template, got {other:?}"),
        }
    }

    #[test]
    fn block_number_predicate_offset_to_template() {
        let config = ValidityPredicateConfig::BlockNumber {
            op: ">=".into(),
            value: None,
            offset: Some(U256::from(10)),
        };
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::BlockNumber { op, bound } => {
                assert_eq!(op, ValidityOperator::GreaterThanOrEqual);
                assert_eq!(bound, BlockNumberBound::Offset(U256::from(10)));
            }
            other => panic!("expected block_number template, got {other:?}"),
        }
    }

    #[test]
    fn block_number_predicate_rejects_both_value_and_offset() {
        let config = ValidityPredicateConfig::BlockNumber {
            op: ">=".into(),
            value: Some(U256::from(1)),
            offset: Some(U256::from(10)),
        };
        let err = config.to_template().unwrap_err();
        assert!(err.to_string().contains("exactly one of 'value' or 'offset'"));
    }

    #[test]
    fn block_number_predicate_rejects_neither_value_nor_offset() {
        let config =
            ValidityPredicateConfig::BlockNumber { op: ">=".into(), value: None, offset: None };
        let err = config.to_template().unwrap_err();
        assert!(err.to_string().contains("exactly one of 'value' or 'offset'"));
    }

    #[test]
    fn block_number_predicate_parses_from_yaml_offset_form() {
        let config: ValidityPredicateConfig =
            serde_yaml::from_str("type: block_number\nop: \">=\"\noffset: \"10\"\n").unwrap();
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::BlockNumber { bound, .. } => {
                assert_eq!(bound, BlockNumberBound::Offset(U256::from(10)));
            }
            other => panic!("expected block_number template, got {other:?}"),
        }
    }

    #[test]
    fn block_number_predicate_parses_from_yaml_value_form() {
        let config: ValidityPredicateConfig =
            serde_yaml::from_str("type: block_number\nop: \">=\"\nvalue: \"12345\"\n").unwrap();
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::BlockNumber { bound, .. } => {
                assert_eq!(bound, BlockNumberBound::Absolute(U256::from(12345)));
            }
            other => panic!("expected block_number template, got {other:?}"),
        }
    }

    #[test]
    fn storage_predicate_parses_from_yaml_hex_form() {
        let config: ValidityPredicateConfig = serde_yaml::from_str(
            "type: storage\naddress: \"0x1234567890123456789012345678901234567890\"\nslot:\n  kind: fixed\n  value: \"0x100\"\nmask: \"0xff\"\nop: \"=\"\nvalue: \"0x1\"\n",
        )
        .unwrap();
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::Storage { slot, mask, op, value, .. } => {
                assert!(matches!(slot, SlotTemplate::Fixed(s) if s == U256::from(0x100)));
                assert_eq!(mask, Some(U256::from(0xffu64)));
                assert_eq!(op, ValidityOperator::Equal);
                assert_eq!(value, U256::from(1u64));
            }
            other => panic!("expected storage template, got {other:?}"),
        }
    }

    #[test]
    fn flashblock_index_predicate_to_template() {
        let config =
            ValidityPredicateConfig::FlashblockIndex { op: "=".into(), value: U256::from(2) };
        match config.to_template().unwrap() {
            ValidityPredicateTemplate::FlashblockIndex { op, value } => {
                assert_eq!(op, ValidityOperator::Equal);
                assert_eq!(value, U256::from(2));
            }
            other => panic!("expected flashblock_index template, got {other:?}"),
        }
    }

    #[test]
    fn position_predicate_surfaces_bad_operator() {
        let config = ValidityPredicateConfig::BlockNumber {
            op: "==".into(),
            value: Some(U256::from(1)),
            offset: None,
        };
        assert!(config.to_template().is_err());
    }

    #[test]
    fn validate_rejects_ratio_above_one() {
        let config = ValidityConfig { ratio: 1.5, ..Default::default() };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_predicates_when_enabled() {
        let config = ValidityConfig { ratio: 0.5, ..Default::default() };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("validity.predicates must be non-empty"));
    }

    #[test]
    fn validate_allows_empty_predicates_when_disabled() {
        let config = ValidityConfig { ratio: 0.0, ..Default::default() };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_too_many_predicates() {
        let predicate = ValidityPredicateConfig::Balance {
            address: PredicateAddressConfig::Sender,
            op: ">=".into(),
            value: U256::ZERO,
        };
        let config = ValidityConfig {
            ratio: 1.0,
            predicates: vec![predicate; DEFAULT_MAX_VALIDITY_PREDICATES + 1],
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("exceeding the maximum"));
    }

    #[test]
    fn validate_surfaces_bad_operator() {
        let config = ValidityConfig {
            ratio: 1.0,
            predicates: vec![ValidityPredicateConfig::Balance {
                address: PredicateAddressConfig::Sender,
                op: "==".into(),
                value: U256::ZERO,
            }],
        };
        assert!(config.validate().is_err());
    }
}
