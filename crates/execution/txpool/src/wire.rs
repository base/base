use alloy_primitives::{Address, Bytes};
use serde::{Deserialize, Serialize};

use crate::TransactionValidity;

/// Error returned when applying extension data to a pooled transaction fails.
#[derive(Debug, thiserror::Error)]
#[error("failed to apply transaction extensions: {0}")]
pub struct ExtensionError(pub String);

/// Pre-validated transaction for the builder RPC wire format.
///
/// Carries the recovered sender address so the builder can skip signer
/// recovery, and the EIP-2718 encoded transaction envelope.
///
/// The `TransactionValidity` parameter carries additional wire fields, flattened into the
/// top-level JSON object. It defaults to [`TransactionValidity`], which serializes to
/// exactly the same bytes as a struct without the field at all, so the default
/// instantiation is wire-compatible in both directions with peers that predate
/// this parameter.
///
/// Legacy [`TransactionValidity`] readers silently ignore extension fields. Any future
/// behavior that relies on extension enforcement must therefore negotiate peer
/// support instead of relying on this compatibility fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedTransaction {
    /// Recovered signer address.
    pub sender: Address,
    /// EIP-2718 encoded transaction bytes.
    pub raw: Bytes,
    /// Extension fields, inlined into the top-level JSON object.
    ///
    /// Deliberately not `#[serde(default)]`: that would force an `TransactionValidity: Default`
    /// bound onto the generated `Deserialize` impl. Flattening already handles
    /// an absent payload, since `TransactionValidity` is deserialized from whatever keys remain.
    #[serde(flatten)]
    pub extensions: TransactionValidity,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;
    use crate::{TransactionValidity, ValidityOperator, ValidityPredicate};

    /// Mirrors the field layout this type had before `extensions` was added, so
    /// the tests below can assert byte-identical encoding against it.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct LegacyValidatedTransaction {
        sender: Address,
        raw: Bytes,
    }

    fn sender() -> Address {
        Address::repeat_byte(0x42)
    }

    fn raw() -> Bytes {
        Bytes::from_static(&[0x02, 0xff, 0x00])
    }

    #[test]
    fn no_extensions_encoding_matches_legacy_layout() {
        let legacy = LegacyValidatedTransaction { sender: sender(), raw: raw() };
        let current = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TransactionValidity::default(),
        };

        assert_eq!(
            serde_json::to_string(&legacy).unwrap(),
            serde_json::to_string(&current).unwrap(),
            "default instantiation must be byte-identical to the pre-generic layout"
        );
    }

    #[test]
    fn empty_validity_encoding_matches_legacy_layout() {
        let legacy = LegacyValidatedTransaction { sender: sender(), raw: raw() };
        let current = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TransactionValidity::default(),
        };

        assert_eq!(serde_json::to_value(legacy).unwrap(), serde_json::to_value(current).unwrap());
    }

    #[test]
    fn storage_validity_is_flattened_and_round_trips() {
        let predicate = ValidityPredicate::Storage {
            address: sender(),
            slot: U256::from(1),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(2),
        };
        let tx = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TransactionValidity { validity: vec![predicate.clone()] },
        };

        let value = serde_json::to_value(&tx).unwrap();
        assert_eq!(value["validity"][0]["type"], "storage");
        assert_eq!(value["validity"][0]["params"]["slot"], "0x1");
        assert!(value.get("extensions").is_none());

        let decoded: ValidatedTransaction = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.extensions.validity, vec![predicate]);
    }

    #[test]
    fn no_extensions_adds_no_json_fields() {
        let tx = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TransactionValidity::default(),
        };

        let json = serde_json::to_string(&tx).unwrap();
        assert!(!json.contains("extensions"), "flattened marker must not emit a key: {json}");
        assert_eq!(
            json,
            r#"{"sender":"0x4242424242424242424242424242424242424242","raw":"0x02ff00"}"#
        );
    }

    #[test]
    fn validity_reader_accepts_legacy_payload() {
        let legacy = LegacyValidatedTransaction { sender: sender(), raw: raw() };
        let decoded: ValidatedTransaction =
            serde_json::from_value(serde_json::to_value(legacy).unwrap()).unwrap();
        assert_eq!(decoded.sender, sender());
        assert!(decoded.extensions.is_empty());
    }
}
