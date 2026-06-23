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
    /// The target block number for inclusion.
    pub block_number: u64,
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

impl ParsedBundle {
    /// Builds a [`ParsedBundle`] from transactions that have already been decoded and
    /// signer-recovered, reusing them instead of decoding the raw bytes again.
    ///
    /// Callers that already hold the recovered transaction (such as the ingress RPC, which
    /// recovers it before wrapping it in a bundle) should use this to avoid a redundant
    /// decode and signature recovery.
    ///
    /// # Errors
    ///
    /// Returns an error if `bundle.replacement_uuid` is set but is not a valid UUID.
    pub fn from_recovered_txs(
        txs: Vec<Recovered<BaseTxEnvelope>>,
        bundle: Bundle,
    ) -> Result<Self, String> {
        let replacement_uuid = bundle
            .replacement_uuid
            .map(|x| Uuid::parse_str(x.as_ref()))
            .transpose()
            .map_err(|e| format!("Invalid UUID: {e:?}"))?;

        Ok(Self {
            txs,
            block_number: bundle.block_number,
            flashblock_number_min: bundle.flashblock_number_min,
            flashblock_number_max: bundle.flashblock_number_max,
            min_timestamp: bundle.min_timestamp,
            max_timestamp: bundle.max_timestamp,
            reverting_tx_hashes: bundle.reverting_tx_hashes,
            replacement_uuid,
            dropping_tx_hashes: bundle.dropping_tx_hashes,
        })
    }
}

impl TryFrom<Bundle> for ParsedBundle {
    type Error = String;

    fn try_from(bundle: Bundle) -> Result<Self, Self::Error> {
        let txs: Vec<Recovered<BaseTxEnvelope>> = bundle
            .txs
            .iter()
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

        Self::from_recovered_txs(txs, bundle)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256};
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
            block_number: 100,
            flashblock_number_min: Some(1),
            flashblock_number_max: Some(5),
            min_timestamp: Some(1000),
            max_timestamp: Some(2000),
            reverting_tx_hashes: vec![],
            replacement_uuid: None,
            dropping_tx_hashes: vec![],
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        assert_eq!(parsed.txs.len(), 1);
        assert_eq!(parsed.block_number, 100);
        assert_eq!(parsed.flashblock_number_min, Some(1));
        assert_eq!(parsed.flashblock_number_max, Some(5));
        assert_eq!(parsed.min_timestamp, Some(1000));
        assert_eq!(parsed.max_timestamp, Some(2000));
        assert!(parsed.replacement_uuid.is_none());
    }

    #[test]
    fn from_recovered_txs_matches_try_from() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 7, bob.address(), U256::from(10_000));
        let tx_bytes: Bytes = tx.encoded_2718().into();

        let bundle = Bundle {
            txs: vec![tx_bytes.clone()],
            block_number: 100,
            max_timestamp: Some(2000),
            ..Default::default()
        };

        // Decode and recover once, then build the parsed bundle from the recovered tx.
        let recovered = BaseTxEnvelope::decode_2718_exact(tx_bytes.as_ref())
            .unwrap()
            .try_into_recovered()
            .unwrap();
        let reused = ParsedBundle::from_recovered_txs(vec![recovered], bundle.clone()).unwrap();

        // Decoding the raw bytes must produce an identical bundle (same fields, same hash).
        let from_bytes: ParsedBundle = bundle.try_into().unwrap();

        assert_eq!(reused, from_bytes);
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
            block_number: 100,
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
            block_number: 100,
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
            block_number: 100,
            replacement_uuid: Some("not-a-valid-uuid".to_string()),
            ..Default::default()
        };

        let result: Result<ParsedBundle, _> = bundle.try_into();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid UUID"));
    }
}
