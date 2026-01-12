//! Accepted bundle type that has been validated and metered.

use alloy_consensus::transaction::Recovered;
use alloy_primitives::TxHash;
use op_alloy_consensus::OpTxEnvelope;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{MeterBundleResponse, ParsedBundle};

/// `AcceptedBundle` is the type that is sent over the wire after validation.
///
/// This represents a bundle that has been decoded, validated, and metered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedBundle {
    /// Unique identifier for this bundle instance.
    pub uuid: Uuid,

    /// Decoded and recovered transactions.
    pub txs: Vec<Recovered<OpTxEnvelope>>,

    /// The target block number for inclusion.
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,

    /// Minimum flashblock number for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_min: Option<u64>,

    /// Maximum flashblock number for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_max: Option<u64>,

    /// Minimum timestamp for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_timestamp: Option<u64>,

    /// Maximum timestamp for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timestamp: Option<u64>,

    /// Transaction hashes that are allowed to revert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverting_tx_hashes: Vec<TxHash>,

    /// UUID for bundle replacement (if this bundle replaces another).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<Uuid>,

    /// Transaction hashes that should be dropped from the pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropping_tx_hashes: Vec<TxHash>,

    /// Metering response from bundle simulation.
    pub meter_bundle_response: MeterBundleResponse,
}

impl AcceptedBundle {
    /// Creates a new accepted bundle from a parsed bundle and metering response.
    pub fn new(bundle: ParsedBundle, meter_bundle_response: MeterBundleResponse) -> Self {
        Self {
            uuid: bundle.replacement_uuid.unwrap_or_else(Uuid::new_v4),
            txs: bundle.txs,
            block_number: bundle.block_number,
            flashblock_number_min: bundle.flashblock_number_min,
            flashblock_number_max: bundle.flashblock_number_max,
            min_timestamp: bundle.min_timestamp,
            max_timestamp: bundle.max_timestamp,
            reverting_tx_hashes: bundle.reverting_tx_hashes,
            replacement_uuid: bundle.replacement_uuid,
            dropping_tx_hashes: bundle.dropping_tx_hashes,
            meter_bundle_response,
        }
    }

    /// Returns the unique identifier of this bundle.
    pub const fn uuid(&self) -> &Uuid {
        &self.uuid
    }
}

impl From<AcceptedBundle> for ParsedBundle {
    fn from(accepted_bundle: AcceptedBundle) -> Self {
        Self {
            txs: accepted_bundle.txs,
            block_number: accepted_bundle.block_number,
            flashblock_number_min: accepted_bundle.flashblock_number_min,
            flashblock_number_max: accepted_bundle.flashblock_number_max,
            min_timestamp: accepted_bundle.min_timestamp,
            max_timestamp: accepted_bundle.max_timestamp,
            reverting_tx_hashes: accepted_bundle.reverting_tx_hashes,
            replacement_uuid: accepted_bundle.replacement_uuid,
            dropping_tx_hashes: accepted_bundle.dropping_tx_hashes,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use alloy_provider::network::eip2718::Encodable2718;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::{
        Bundle,
        test_utils::{create_test_meter_bundle_response, create_transaction},
    };

    #[test]
    fn test_accepted_bundle_new() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx_hash = tx.tx_hash();
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()], block_number: 100, ..Default::default() };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let meter_response = create_test_meter_bundle_response();
        let accepted = AcceptedBundle::new(parsed, meter_response);

        assert!(!accepted.uuid().is_nil());
        assert!(accepted.replacement_uuid.is_none());
        assert_eq!(accepted.txs.len(), 1);
        assert_eq!(accepted.txs[0].tx_hash(), tx_hash);
        assert_eq!(accepted.block_number, 100);
    }

    #[test]
    fn test_accepted_bundle_with_replacement_uuid() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let uuid = Uuid::new_v4();
        let bundle = Bundle {
            txs: vec![tx_bytes.into()],
            block_number: 100,
            replacement_uuid: Some(uuid.to_string()),
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let meter_response = create_test_meter_bundle_response();
        let accepted = AcceptedBundle::new(parsed, meter_response);

        assert_eq!(*accepted.uuid(), uuid);
        assert_eq!(accepted.replacement_uuid, Some(uuid));
    }

    #[test]
    fn test_accepted_bundle_to_parsed_bundle() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx_hash = tx.tx_hash();
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle {
            txs: vec![tx_bytes.into()],
            block_number: 100,
            flashblock_number_min: Some(1),
            flashblock_number_max: Some(5),
            min_timestamp: Some(1000),
            max_timestamp: Some(2000),
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let meter_response = create_test_meter_bundle_response();
        let accepted = AcceptedBundle::new(parsed, meter_response);

        let back_to_parsed: ParsedBundle = accepted.into();
        assert_eq!(back_to_parsed.txs.len(), 1);
        assert_eq!(back_to_parsed.txs[0].tx_hash(), tx_hash);
        assert_eq!(back_to_parsed.block_number, 100);
        assert_eq!(back_to_parsed.flashblock_number_min, Some(1));
        assert_eq!(back_to_parsed.flashblock_number_max, Some(5));
        assert_eq!(back_to_parsed.min_timestamp, Some(1000));
        assert_eq!(back_to_parsed.max_timestamp, Some(2000));
    }
}
