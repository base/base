//! Accepted bundle type that has been validated and metered.

use alloy_consensus::transaction::Recovered;
use base_common_consensus::BaseTxEnvelope;
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
    pub txs: Vec<Recovered<BaseTxEnvelope>>,

    /// Metering response from bundle simulation.
    pub meter_bundle_response: MeterBundleResponse,
}

impl AcceptedBundle {
    /// Creates a new accepted bundle from a parsed bundle and metering response.
    pub fn new(bundle: ParsedBundle, meter_bundle_response: MeterBundleResponse) -> Self {
        Self { uuid: Uuid::new_v4(), txs: bundle.txs, meter_bundle_response }
    }

    /// Returns the unique identifier of this bundle.
    pub const fn uuid(&self) -> &Uuid {
        &self.uuid
    }
}

impl From<AcceptedBundle> for ParsedBundle {
    fn from(accepted_bundle: AcceptedBundle) -> Self {
        Self { txs: accepted_bundle.txs }
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

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_hash = tx.tx_hash();
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()] };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let meter_response = create_test_meter_bundle_response();
        let accepted = AcceptedBundle::new(parsed, meter_response);

        assert!(!accepted.uuid().is_nil());
        assert_eq!(accepted.txs.len(), 1);
        assert_eq!(accepted.txs[0].tx_hash(), tx_hash);
    }

    #[test]
    fn test_accepted_bundle_to_parsed_bundle() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_hash = tx.tx_hash();
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()] };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let meter_response = create_test_meter_bundle_response();
        let accepted = AcceptedBundle::new(parsed, meter_response);

        let back_to_parsed: ParsedBundle = accepted.into();
        assert_eq!(back_to_parsed.txs.len(), 1);
        assert_eq!(back_to_parsed.txs[0].tx_hash(), tx_hash);
    }
}
