use core::fmt::Debug;

use alloy_primitives::{Address, Bytes};
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Default extension payload for [`ValidatedTransaction`], contributing no
/// additional wire fields.
///
/// This must remain a braced empty struct. A unit struct (`struct NoExtensions;`)
/// serializes to `null`, which `#[serde(flatten)]` rejects at runtime rather than
/// at compile time.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoExtensions {}

/// Error returned when applying extension data to a pooled transaction fails.
#[derive(Debug, thiserror::Error)]
#[error("failed to apply transaction extensions: {0}")]
pub struct ExtensionError(pub String);

/// Pluggable extension payload carried alongside a [`ValidatedTransaction`].
///
/// `T` is the pooled transaction type the extensions decorate. This crate only
/// ever names [`NoExtensions`]; downstream node builds substitute their own
/// payload type to carry additional per-transaction data over the builder RPC.
///
/// Implementors are constrained by `#[serde(flatten)]`: the payload must
/// serialize as a JSON map, and it must not use `u128`/`i128` fields, which
/// `serde_json` cannot represent through flattening.
pub trait ValidatedTransactionExtensions<T: PoolTransaction>:
    Serialize + DeserializeOwned + Debug + Clone + Send + Sync + Unpin + 'static
{
    /// Returns whether the payload carries no extension data.
    ///
    /// Builders use this to preserve legacy transaction handling while rejecting
    /// or privately inserting non-empty extension payloads.
    fn is_empty(&self) -> bool {
        false
    }

    /// Validates the payload against the configured maximum number of extension items.
    ///
    /// Called by the builder before decoding and applying the payload. Each extension
    /// type defines what constitutes one item.
    fn validate(&self, _max_items: usize) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Extracts extension data from an outbound pooled transaction.
    ///
    /// Called by the forwarder for each transaction it relays to a builder.
    fn extract(tx: &ValidPoolTransaction<T>) -> Self;

    /// Applies extension data to an inbound pooled transaction.
    ///
    /// Called by the builder RPC handler before the transaction is inserted
    /// into the pool.
    fn apply(self, tx: T) -> Result<T, ExtensionError>;
}

impl<T: PoolTransaction> ValidatedTransactionExtensions<T> for NoExtensions {
    fn is_empty(&self) -> bool {
        true
    }

    fn extract(_tx: &ValidPoolTransaction<T>) -> Self {
        Self {}
    }

    fn apply(self, tx: T) -> Result<T, ExtensionError> {
        Ok(tx)
    }
}

/// Pre-validated transaction for the builder RPC wire format.
///
/// Carries the recovered sender address so the builder can skip signer
/// recovery, and the EIP-2718 encoded transaction envelope.
///
/// The `E` parameter carries additional wire fields, flattened into the
/// top-level JSON object. It defaults to [`NoExtensions`], which serializes to
/// exactly the same bytes as a struct without the field at all, so the default
/// instantiation is wire-compatible in both directions with peers that predate
/// this parameter.
///
/// Legacy [`NoExtensions`] readers silently ignore extension fields. Any future
/// behavior that relies on extension enforcement must therefore negotiate peer
/// support instead of relying on this compatibility fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedTransaction<E = NoExtensions> {
    /// Recovered signer address.
    pub sender: Address,
    /// EIP-2718 encoded transaction bytes.
    pub raw: Bytes,
    /// Extension fields, inlined into the top-level JSON object.
    ///
    /// Deliberately not `#[serde(default)]`: that would force an `E: Default`
    /// bound onto the generated `Deserialize` impl. Flattening already handles
    /// an absent payload, since `E` is deserialized from whatever keys remain.
    #[serde(flatten)]
    pub extensions: E,
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

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
    struct TestExtensions {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        extra: Option<u64>,
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
        let current =
            ValidatedTransaction { sender: sender(), raw: raw(), extensions: NoExtensions {} };

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

        let decoded: ValidatedTransaction<TransactionValidity> =
            serde_json::from_value(value).unwrap();
        assert_eq!(decoded.extensions.validity, vec![predicate]);
    }

    #[test]
    fn no_extensions_adds_no_json_fields() {
        let tx = ValidatedTransaction { sender: sender(), raw: raw(), extensions: NoExtensions {} };

        let json = serde_json::to_string(&tx).unwrap();
        assert!(!json.contains("extensions"), "flattened marker must not emit a key: {json}");
        assert_eq!(
            json,
            r#"{"sender":"0x4242424242424242424242424242424242424242","raw":"0x02ff00"}"#
        );
    }

    #[test]
    fn extension_fields_inline_at_top_level() {
        let tx = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TestExtensions { extra: Some(9) },
        };

        let json = serde_json::to_string(&tx).unwrap();
        assert!(json.contains(r#""extra":9"#), "extension field must be inlined: {json}");
    }

    #[test]
    fn default_reader_accepts_payload_carrying_extensions() {
        let extended = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TestExtensions { extra: Some(9) },
        };
        let json = serde_json::to_string(&extended).unwrap();

        let decoded: ValidatedTransaction = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.sender, sender());
        assert_eq!(decoded.extensions, NoExtensions {});
    }

    #[test]
    fn extension_reader_accepts_payload_without_extensions() {
        let legacy = LegacyValidatedTransaction { sender: sender(), raw: raw() };
        let json = serde_json::to_string(&legacy).unwrap();

        let decoded: ValidatedTransaction<TestExtensions> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.sender, sender());
        assert_eq!(decoded.extensions, TestExtensions { extra: None });
    }

    #[test]
    fn large_u64_survives_flatten_buffering() {
        let tx = ValidatedTransaction {
            sender: sender(),
            raw: raw(),
            extensions: TestExtensions { extra: Some(u64::MAX) },
        };
        let json = serde_json::to_string(&tx).unwrap();

        let decoded: ValidatedTransaction<TestExtensions> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.extensions.extra, Some(u64::MAX));
    }
}
