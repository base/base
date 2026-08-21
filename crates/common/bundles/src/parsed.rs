//! Parsed bundle type with decoded transactions.

use alloy_consensus::transaction::{Recovered, SignerRecoverable};
use alloy_primitives::TxHash;
use alloy_provider::network::eip2718::Decodable2718;
use base_common_consensus::BaseTxEnvelope;
use uuid::Uuid;

use crate::Bundle;

/// `ParsedBundle` is the type that contains utility methods for the `Bundle` type.
///
/// Unlike [`Bundle`], this type has decoded transactions with recovered signers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBundle {
    /// Decoded and recovered transactions.
    pub txs: Vec<Recovered<BaseTxEnvelope>>,
    /// Minimum block number for inclusion.
    pub min_block_number: Option<u64>,
    /// Maximum block number for inclusion.
    pub max_block_number: Option<u64>,
    /// Minimum flashblock number for inclusion.
    pub flashblock_number_min: Option<u64>,
    /// Maximum flashblock number for inclusion.
    pub flashblock_number_max: Option<u64>,
    /// Minimum timestamp for inclusion.
    pub min_timestamp: Option<u64>,
    /// Maximum timestamp for inclusion.
    pub max_timestamp: Option<u64>,
    /// Transaction hashes that are allowed to revert.
    pub reverting_tx_hashes: Vec<TxHash>,
    /// UUID for bundle replacement.
    pub replacement_uuid: Option<Uuid>,
    /// Transaction hashes that should be dropped from the pool.
    pub dropping_tx_hashes: Vec<TxHash>,
}

impl TryFrom<Bundle> for ParsedBundle {
    type Error = String;

    fn try_from(bundle: Bundle) -> Result<Self, Self::Error> {
        let txs: Vec<Recovered<BaseTxEnvelope>> = bundle
            .txs
            .into_iter()
            .map(|tx| {
                BaseTxEnvelope::decode_2718_exact(tx.iter().as_slice())
                    .map_err(|e| format!("Failed to decode transaction: {e:?}"))
                    .and_then(|tx| {
                        tx.try_into_recovered().map_err(|e| {
                            format!("Failed to convert transaction to recovered: {e:?}")
                        })
                    })
            })
            .collect::<Result<Vec<Recovered<BaseTxEnvelope>>, String>>()?;

        let uuid = bundle
            .replacement_uuid
            .map(|x| Uuid::parse_str(x.as_ref()))
            .transpose()
            .map_err(|e| format!("Invalid UUID: {e:?}"))?;

        Ok(Self {
            txs,
            min_block_number: bundle.min_block_number,
            max_block_number: bundle.max_block_number,
            flashblock_number_min: bundle.flashblock_number_min,
            flashblock_number_max: bundle.flashblock_number_max,
            min_timestamp: bundle.min_timestamp,
            max_timestamp: bundle.max_timestamp,
            reverting_tx_hashes: bundle.reverting_tx_hashes,
            replacement_uuid: uuid,
            dropping_tx_hashes: bundle.dropping_tx_hashes,
        })
    }
}

impl ParsedBundle {
    /// Creates a one-transaction bundle from a recovered envelope.
    pub fn from_recovered(tx: Recovered<BaseTxEnvelope>) -> Self {
        Self {
            txs: vec![tx],
            min_block_number: None,
            max_block_number: None,
            flashblock_number_min: None,
            flashblock_number_max: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: Vec::new(),
            replacement_uuid: None,
            dropping_tx_hashes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::SignerRecoverable;
    use alloy_primitives::U256;
    use alloy_provider::network::eip2718::Encodable2718;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::test_utils::create_transaction;

    #[test]
    fn test_parsed_bundle_from_bundle() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle {
            txs: vec![tx_bytes.into()],
            min_block_number: Some(100),
            max_block_number: Some(105),
            flashblock_number_min: Some(1),
            flashblock_number_max: Some(5),
            min_timestamp: Some(1000),
            max_timestamp: Some(2000),
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        assert_eq!(parsed.txs.len(), 1);
        assert_eq!(parsed.min_block_number, Some(100));
        assert_eq!(parsed.max_block_number, Some(105));
        assert_eq!(parsed.flashblock_number_min, Some(1));
        assert_eq!(parsed.flashblock_number_max, Some(5));
        assert_eq!(parsed.min_timestamp, Some(1000));
        assert_eq!(parsed.max_timestamp, Some(2000));
        assert!(parsed.replacement_uuid.is_none());
    }

    #[test]
    fn from_recovered_wraps_a_single_transaction() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();
        let envelope = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let recovered = envelope.clone().try_into_recovered().unwrap();
        let hash = *recovered.hash();

        let parsed = ParsedBundle::from_recovered(recovered);
        assert_eq!(parsed.txs.len(), 1, "from_recovered must wrap exactly one transaction");
        assert_eq!(*parsed.txs[0].hash(), hash);
        assert!(parsed.min_block_number.is_none());
        assert!(parsed.replacement_uuid.is_none());
    }

    #[test]
    fn test_parsed_bundle_with_uuid() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let uuid = Uuid::new_v4();
        let bundle = Bundle {
            txs: vec![tx_bytes.into()],
            min_block_number: Some(100),
            max_block_number: Some(100),
            replacement_uuid: Some(uuid.to_string()),
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        assert_eq!(parsed.replacement_uuid, Some(uuid));
    }

    #[test]
    fn test_parsed_bundle_invalid_tx() {
        let bundle = Bundle {
            txs: vec![vec![0x00, 0x01, 0x02].into()],
            min_block_number: Some(100),
            max_block_number: Some(100),
            ..Default::default()
        };

        let result: Result<ParsedBundle, _> = bundle.try_into();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to decode transaction"));
    }

    #[test]
    fn test_parsed_bundle_invalid_uuid() {
        let bundle = Bundle {
            txs: vec![],
            min_block_number: Some(100),
            max_block_number: Some(100),
            replacement_uuid: Some("not-a-valid-uuid".to_string()),
            ..Default::default()
        };

        let result: Result<ParsedBundle, _> = bundle.try_into();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid UUID"));
    }
}
